// rmcp deprecated the MCP Roots feature (SEP-2577), but this client still
// implements roots support that connected MCP servers rely on. Keep using the
// deprecated APIs until roots support is dropped here or removed upstream.
#![allow(deprecated)]

use crate::config::{McpServerConfig, ToolDisplayTemplates};
use crate::convert::{mcp_tool_to_declaration, ToolTemplates};
use crate::safety::path_to_file_uri;
use harnx_core::abort::{wait_abort_signal, AbortSignal};
use harnx_core::event::NoticeEvent;
use harnx_core::sink::emit_agent_event;
use harnx_core::tool::{ToolDeclaration, ToolError, ToolProvider};

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use globset::GlobBuilder;
use parking_lot::RwLock;
use process_wrap::tokio::CommandWrap;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
use rmcp::handler::client::ClientHandler;
use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ErrorData, Implementation, InitializeRequestParams,
    ListRootsResult, Root,
};
use rmcp::service::{RequestContext, RoleClient, RunningService, ServiceError};
use serde_json::Value;
use std::collections::VecDeque;
use std::process::Stdio;
use std::time::Duration;
use std::{
    collections::{HashMap, HashSet},
    fmt,
    path::Path,
    sync::Arc,
};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::runtime::{Builder, Handle, RuntimeFlavor};

/// Maximum number of stderr lines to keep from an MCP child process for
/// inclusion in connection error messages. Old lines are dropped first.
const MCP_STDERR_TAIL_LINES: usize = 64;

/// Minimum time between duplicate notices for the same server+key.
const NOTICE_DEDUP_WINDOW: Duration = Duration::from_secs(5);

/// Shared per-client dedup state: the (key, timestamp) of the last emitted
/// notice. Wrapped in `Arc` so the background child-wait task can share the
/// same state as the synchronous reconnect path — this keeps notice
/// deduplication consistent across reconnects rather than resetting per
/// connection.
type NoticeDedupState = Arc<RwLock<Option<(String, std::time::Instant)>>>;

/// Emit a `NoticeEvent` through the global agent-event sink, suppressing a
/// duplicate `(key)` within `NOTICE_DEDUP_WINDOW`. Returns true if emitted,
/// false if suppressed. Shared by `McpClient::emit_notice` and the detached
/// child-wait task so both honour the same dedup window.
fn emit_notice_dedup(state: &NoticeDedupState, key: &str, event: NoticeEvent) -> bool {
    let now = std::time::Instant::now();
    let mut last_notice = state.write();
    if let Some((ref last_key, ref last_time)) = *last_notice {
        if last_key == key && now.duration_since(*last_time) < NOTICE_DEDUP_WINDOW {
            return false;
        }
    }
    *last_notice = Some((key.to_string(), now));
    drop(last_notice);
    emit_agent_event(harnx_core::event::AgentEvent::Notice(event));
    true
}

// --- Part B: exit status classification -------------------------------------

/// Classify exit status into a NoticeEvent.
/// - Clean exit (code 0, or SIGTERM/SIGINT on Unix) → Warning
/// - Nonzero code or SIGKILL/other signals → Error
/// - No status available → Error
#[cfg(unix)]
pub fn classify_exit(name: &str, code: Option<i32>, signal: Option<i32>) -> NoticeEvent {
    // Check signal first (if killed by signal)
    if let Some(sig) = signal {
        // Common signal mappings
        match sig {
            15 | 2 => NoticeEvent::Warning(format!(
                "MCP server '{}' terminated by SIG{}",
                name,
                if sig == 15 { "TERM" } else { "INT" }
            )),
            9 => NoticeEvent::Error(format!("MCP server '{}' killed by SIGKILL", name)),
            _ => NoticeEvent::Error(format!("MCP server '{}' died: signal {}", name, sig)),
        }
    } else if let Some(c) = code {
        if c == 0 {
            NoticeEvent::Warning(format!("MCP server '{}' exited cleanly", name))
        } else {
            NoticeEvent::Error(format!("MCP server '{}' exited with code {}", name, c))
        }
    } else {
        NoticeEvent::Error(format!("MCP server '{}' exited (status unavailable)", name))
    }
}

#[cfg(not(unix))]
pub fn classify_exit(name: &str, code: Option<i32>, _signal: Option<i32>) -> NoticeEvent {
    if let Some(c) = code {
        if c == 0 {
            NoticeEvent::Warning(format!("MCP server '{}' exited cleanly", name))
        } else {
            NoticeEvent::Error(format!("MCP server '{}' exited with code {}", name, c))
        }
    } else {
        NoticeEvent::Error(format!("MCP server '{}' exited (status unavailable)", name))
    }
}

/// How long to wait for the stderr reader to drain after a connection
/// error before snapshotting the buffer for the error message. Short
/// enough to keep startup snappy, long enough that a child that exited
/// just before initialize completed has time to flush.
const MCP_STDERR_DRAIN_DELAY: Duration = Duration::from_millis(150);

type StderrBuffer = Arc<parking_lot::Mutex<VecDeque<String>>>;

fn new_stderr_buffer() -> StderrBuffer {
    Arc::new(parking_lot::Mutex::new(VecDeque::with_capacity(
        MCP_STDERR_TAIL_LINES,
    )))
}

fn render_stderr_tail(buffer: &StderrBuffer) -> String {
    let buf = buffer.lock();
    if buf.is_empty() {
        return String::new();
    }
    let joined = buf.iter().cloned().collect::<Vec<_>>().join("\n");
    format!("\nMCP server stderr:\n{joined}")
}

async fn snapshot_stderr_tail(buffer: &StderrBuffer) -> String {
    tokio::time::sleep(MCP_STDERR_DRAIN_DELAY).await;
    render_stderr_tail(buffer)
}

pub struct McpClient {
    name: String,
    config: McpServerConfig,
    tools: Arc<RwLock<Vec<ToolDeclaration>>>,
    roots: Arc<RwLock<Vec<String>>>,
    connected: Arc<RwLock<bool>>,
    connection_failed: Arc<RwLock<bool>>,
    service: Arc<RwLock<Option<RunningService<RoleClient, McpClientHandler>>>>,
    /// Persistent stderr buffer for notice messages (survives reconnects)
    stderr_buffer: StderrBuffer,
    /// Last emitted notice (key, timestamp) for deduplication. Shared with the
    /// background child-wait task so exit and reconnect notices dedup together.
    last_notice: NoticeDedupState,
    /// Handle to the background task that waits for the current child process
    /// and emits an exit notice. Aborted and replaced on each (re)connect so a
    /// stale task from a prior connection cannot linger past teardown.
    child_wait_task: RwLock<Option<tokio::task::JoinHandle<()>>>,
    /// PID of the currently-running child process. Captured at spawn and
    /// cleared when the child exits. Surfaced by `.mcp info` for diagnostics.
    pid: Arc<RwLock<Option<u32>>>,
}

#[derive(Clone)]
pub struct McpClientHandler {
    roots: Arc<RwLock<Vec<String>>>,
}

impl McpClientHandler {
    pub fn new(roots: Arc<RwLock<Vec<String>>>) -> Self {
        Self { roots }
    }
}

impl ClientHandler for McpClientHandler {
    fn get_info(&self) -> InitializeRequestParams {
        InitializeRequestParams::new(
            ClientCapabilities::builder()
                .enable_roots()
                .enable_roots_list_changed()
                .build(),
            Implementation::new("harnx", env!("CARGO_PKG_VERSION")),
        )
    }

    async fn list_roots(
        &self,
        _cx: RequestContext<RoleClient>,
    ) -> Result<ListRootsResult, ErrorData> {
        let roots = self.roots.read();
        let roots = roots
            .iter()
            .filter_map(|r| {
                let path = Path::new(r);
                let abs = path
                    .canonicalize()
                    .or_else(|_| std::env::current_dir().map(|cwd| cwd.join(path)))
                    .ok()?;
                Some(Root::new(path_to_file_uri(&abs)))
            })
            .collect();
        Ok(ListRootsResult::new(roots))
    }
}

impl fmt::Debug for McpClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let service = if self.service.read().is_some() {
            "<running-service>"
        } else {
            "<disconnected>"
        };

        f.debug_struct("McpClient")
            .field("name", &self.name)
            .field("config", &self.config)
            .field("tools", &*self.tools.read())
            .field("roots", &*self.roots.read())
            .field("connected", &*self.connected.read())
            .field("service", &service)
            .field("stderr_buffer_len", &self.stderr_buffer.lock().len())
            .finish()
    }
}

impl McpClient {
    fn expand_path(path: &str) -> String {
        shellexpand::full(path)
            .map(|p| p.to_string())
            .unwrap_or_else(|_| path.to_string())
    }

    /// Emit a notice event with deduplication against this client's shared
    /// dedup state. Returns true if emitted, false if suppressed as duplicate.
    fn emit_notice(&self, key: &str, event: NoticeEvent) -> bool {
        emit_notice_dedup(&self.last_notice, key, event)
    }

    pub fn new(config: McpServerConfig) -> Self {
        let name = config.name.clone();
        let roots = config
            .roots
            .iter()
            .map(|r| Self::expand_path(r))
            .collect::<Vec<_>>();
        Self {
            name,
            config,
            tools: Arc::new(RwLock::new(Vec::new())),
            roots: Arc::new(RwLock::new(roots)),
            connected: Arc::new(RwLock::new(false)),
            connection_failed: Arc::new(RwLock::new(false)),
            service: Arc::new(RwLock::new(None)),
            stderr_buffer: new_stderr_buffer(),
            last_notice: Arc::new(RwLock::new(None)),
            child_wait_task: RwLock::new(None),
            pid: Arc::new(RwLock::new(None)),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the hooks configuration for this server, if any.
    /// Used by the runtime to build per-server hook dispatch tables.
    pub fn hooks(&self) -> Option<&harnx_core::hooks::HooksConfig> {
        self.config.hooks.as_ref()
    }

    /// Name of the package this server belongs to, if it came from an installed
    /// package. Used to resolve `HARNX_PACKAGE_DIR` for the server's hooks.
    pub fn package(&self) -> Option<&str> {
        self.config.package.as_deref()
    }

    /// Full resolved server configuration (command, args, env, roots, hooks).
    /// Surfaced by `.mcp info` for diagnostics.
    pub fn config(&self) -> &McpServerConfig {
        &self.config
    }

    /// PID of the running child process, or `None` when not connected.
    pub fn pid(&self) -> Option<u32> {
        *self.pid.read()
    }

    /// Human-readable connection status for diagnostics.
    pub fn status_label(&self) -> &'static str {
        if self.is_connected() {
            "connected"
        } else if self.connection_failed() {
            "failed"
        } else {
            "idle"
        }
    }

    /// The effective roots (config roots plus any injected cwd/extra roots).
    pub fn live_roots(&self) -> Vec<String> {
        self.roots.read().clone()
    }

    pub fn is_connected(&self) -> bool {
        *self.connected.read()
    }

    pub fn connection_failed(&self) -> bool {
        *self.connection_failed.read()
    }

    pub async fn connect(&self) -> Result<()> {
        *self.connection_failed.write() = false;
        if self.is_connected() {
            return Ok(());
        }

        match self.connect_inner().await {
            Ok(()) => Ok(()),
            Err(err) => {
                *self.connection_failed.write() = true;
                Err(err)
            }
        }
    }

    async fn connect_inner(&self) -> Result<()> {
        let mut command = Command::new(&self.config.command);
        command.args(&self.config.args);

        command.envs(&self.config.env);

        // Pipe stdin/stdout/stderr for MCP transport
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        // Spawn in a new process group so SIGINT (Ctrl+C) in the parent
        // terminal doesn't propagate to MCP server child processes.
        #[allow(unused_mut)]
        let mut wrap = CommandWrap::from(command);
        #[cfg(unix)]
        wrap.wrap(ProcessGroup::leader());

        // Spawn the child process ourselves (instead of using TokioChildProcess)
        // so we can own the child handle and wait for exit.
        let mut child = wrap
            .spawn()
            .with_context(|| format!("Failed to spawn MCP server '{}'", self.name))?;

        let stdin = child
            .stdin()
            .take()
            .ok_or_else(|| anyhow!("MCP server '{}' stdin not piped", self.name))?;
        let stdout = child
            .stdout()
            .take()
            .ok_or_else(|| anyhow!("MCP server '{}' stdout not piped", self.name))?;
        let stderr = child.stderr().take();

        // Record the child PID for diagnostics; cleared by the wait task on exit.
        *self.pid.write() = child.id();

        // Spawn a background task that waits for the child and emits an exit
        // notice. It shares the client's `last_notice` dedup state so exit and
        // reconnect notices are deduplicated together. Any wait task from a
        // prior connection is aborted first so stale tasks don't linger.
        let name_for_wait = self.name.clone();
        let stderr_buffer_for_wait = self.stderr_buffer.clone();
        let dedup_state = self.last_notice.clone();
        let pid_for_wait = self.pid.clone();
        let wait_handle = tokio::spawn(async move {
            let wait_result = child.wait().await;
            *pid_for_wait.write() = None;
            if let Ok(status) = wait_result {
                #[cfg(unix)]
                let (code, signal) = {
                    use std::os::unix::process::ExitStatusExt;
                    (status.code(), status.signal())
                };
                #[cfg(not(unix))]
                let (code, signal) = (status.code(), None);

                // Snapshot stderr tail for the notice message
                let stderr_tail = {
                    let buf = stderr_buffer_for_wait.lock();
                    if buf.is_empty() {
                        String::new()
                    } else {
                        let joined = buf.iter().cloned().collect::<Vec<_>>().join("\n");
                        format!("\nMCP server stderr:\n{joined}")
                    }
                };

                let event = match classify_exit(&name_for_wait, code, signal) {
                    NoticeEvent::Warning(msg) if !stderr_tail.is_empty() => {
                        NoticeEvent::Warning(format!("{}\n{}", msg, stderr_tail))
                    }
                    NoticeEvent::Error(msg) if !stderr_tail.is_empty() => {
                        NoticeEvent::Error(format!("{}\n{}", msg, stderr_tail))
                    }
                    other => other,
                };
                let key = format!("exit:{}", name_for_wait);
                emit_notice_dedup(&dedup_state, &key, event);
            }
        });
        if let Some(prev) = self.child_wait_task.write().replace(wait_handle) {
            // Detach old wait task so it can finish child.wait().await and reap old process.
            drop(prev);
        }

        // Spawn stderr reader task using the persistent buffer
        if let Some(stderr) = stderr {
            let server_name = self.name.clone();
            let buffer = self.stderr_buffer.clone();
            tokio::spawn(async move {
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    log::debug!("[mcp:{}] {}", server_name, line);
                    let mut buf = buffer.lock();
                    if buf.len() == MCP_STDERR_TAIL_LINES {
                        buf.pop_front();
                    }
                    buf.push_back(line);
                }
            });
        }

        // Create transport from (stdout, stdin) pair
        let transport =
            rmcp::transport::async_rw::AsyncRwTransport::<RoleClient, _, _>::new(stdout, stdin);

        let handler = McpClientHandler::new(self.roots.clone());
        let serve_result = tokio::time::timeout(
            Duration::from_secs(30),
            rmcp::service::serve_client(handler, transport),
        )
        .await;
        let service = match serve_result {
            Err(_) => {
                let tail = snapshot_stderr_tail(&self.stderr_buffer).await;
                bail!(
                    "MCP server '{}' timed out during initialization (30s){}",
                    self.name,
                    tail,
                );
            }
            Ok(Err(err)) => {
                let tail = snapshot_stderr_tail(&self.stderr_buffer).await;
                return Err(anyhow::Error::from(err)).with_context(|| {
                    format!(
                        "Failed to initialize MCP client for server '{}'{}",
                        self.name, tail,
                    )
                });
            }
            Ok(Ok(service)) => service,
        };

        let list_result = tokio::time::timeout(
            Duration::from_secs(10),
            service.peer().list_tools(Default::default()),
        )
        .await;
        let tools_result = match list_result {
            Err(_) => {
                let tail = snapshot_stderr_tail(&self.stderr_buffer).await;
                bail!(
                    "MCP server '{}' timed out listing tools (10s){}",
                    self.name,
                    tail,
                );
            }
            Ok(Err(err)) => {
                let tail = snapshot_stderr_tail(&self.stderr_buffer).await;
                return Err(anyhow::Error::from(err)).with_context(|| {
                    format!(
                        "Failed to list tools for MCP server '{}'{}",
                        self.name, tail
                    )
                });
            }
            Ok(Ok(result)) => result,
        };
        let functions = tools_result
            .tools
            .into_iter()
            .map(|tool| self.build_tool_declaration(tool))
            .collect::<Result<Vec<_>>>()?;

        *self.tools.write() = functions;
        *self.connected.write() = true;
        *self.service.write() = Some(service);

        Ok(())
    }

    /// Convert a single MCP `Tool` advertised by the server into a harnx
    /// `ToolDeclaration`, applying tool renames and call/result display
    /// templates (server `_meta` overridden by local config).
    fn build_tool_declaration(&self, tool: rmcp::model::Tool) -> Result<ToolDeclaration> {
        let input_schema = Value::Object((*tool.input_schema).clone());
        let server_tool_name = tool.name.to_string();
        let final_name =
            if let Some(renamed) = self.config.rename_tools.get(server_tool_name.as_str()) {
                renamed.clone()
            } else {
                format!("{}_{}", self.name, server_tool_name)
            };

        // Extract _meta templates (server-provided)
        let meta_call_tmpl = tool
            .meta
            .as_ref()
            .and_then(|m| m.0.get("call_template"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let meta_result_tmpl = tool
            .meta
            .as_ref()
            .and_then(|m| m.0.get("result_template"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Apply config override (higher precedence)
        let cfg_templates: Option<&ToolDisplayTemplates> =
            self.config.tool_templates.get(&server_tool_name);
        let call_template = cfg_templates
            .and_then(|t| t.call_template.clone())
            .or(meta_call_tmpl);
        let result_template = cfg_templates
            .and_then(|t| t.result_template.clone())
            .or(meta_result_tmpl);
        let templates = ToolTemplates {
            call_template,
            result_template,
        };

        let mut declaration = mcp_tool_to_declaration(
            &final_name,
            &server_tool_name,
            tool.description.as_deref().unwrap_or_default(),
            &input_schema,
            templates,
            tool.annotations.as_ref(),
        )?;
        declaration.mcp_server_name = Some(self.name.clone());
        Ok(declaration)
    }

    pub async fn disconnect(&self) -> Result<()> {
        let service = self.service.write().take();

        *self.connected.write() = false;
        self.tools.write().clear();

        if let Some(service) = service {
            service
                .cancel()
                .await
                .with_context(|| format!("Failed to disconnect MCP server '{}'", self.name))?;
        }

        Ok(())
    }

    fn invalidate_service(&self) {
        self.service.write().take();
        *self.connected.write() = false;
    }

    pub fn get_tools(&self) -> Vec<ToolDeclaration> {
        self.tools.read().clone()
    }

    pub fn get_roots(&self) -> Vec<String> {
        self.roots.read().clone()
    }

    pub async fn add_root(&self, root: &str) -> Result<()> {
        let root = Self::expand_path(root);
        let changed = {
            let mut roots = self.roots.write();
            if !roots.contains(&root) {
                roots.push(root);
                true
            } else {
                false
            }
        };
        if changed {
            let peer = self.service.read().as_ref().map(|s| s.peer().clone());
            if let Some(peer) = peer {
                let _ = peer.notify_roots_list_changed().await;
            }
        }
        Ok(())
    }

    pub async fn remove_root(&self, root: &str) -> Result<()> {
        let changed = {
            let mut roots = self.roots.write();
            let old_len = roots.len();
            roots.retain(|r| r != root);
            roots.len() < old_len
        };
        if changed {
            let peer = self.service.read().as_ref().map(|s| s.peer().clone());
            if let Some(peer) = peer {
                let _ = peer.notify_roots_list_changed().await;
            }
        }
        Ok(())
    }

    pub async fn call_tool(&self, tool_name: &str, arguments: Value) -> Result<Value> {
        if !self.is_connected() {
            self.connect().await?;
        }

        let arguments = match arguments {
            Value::Null => None,
            Value::Object(arguments) => Some(arguments),
            _ => bail!("MCP tool arguments must be a JSON object or null"),
        };

        let mut params = CallToolRequestParams::new(tool_name.to_string());
        if let Some(args) = arguments {
            params = params.with_arguments(args);
        }

        let peer = {
            let service_guard = self.service.read();
            service_guard
                .as_ref()
                .ok_or_else(|| anyhow!("MCP server '{}' is not connected", self.name))?
                .peer()
                .clone()
        };

        let result = peer.call_tool(params).await;

        match result {
            Ok(result) => {
                serde_json::to_value(result).context("Failed to serialize MCP tool result")
            }
            Err(err) => match err {
                ServiceError::TransportSend(_) | ServiceError::TransportClosed => {
                    log::warn!(
                        "MCP tool '{}' on '{}' transport failed, attempting reconnect: {}",
                        tool_name,
                        self.name,
                        err,
                    );

                    // Snapshot stderr tail BEFORE reconnect for the notice
                    let stderr_tail = render_stderr_tail(&self.stderr_buffer);

                    *self.connected.write() = false;
                    self.service.write().take();

                    // Emit Warning notice about the transport failure and reconnect attempt
                    self.emit_notice(
                        &format!("reconnect:{}", self.name),
                        NoticeEvent::Warning(format!(
                            "MCP server '{}' disconnected, reconnecting{}",
                            self.name,
                            if stderr_tail.is_empty() {
                                String::new()
                            } else {
                                format!(" ({})", stderr_tail.trim())
                            }
                        )),
                    );

                    // Heal the connection for future calls (best-effort)
                    if let Err(reconnect_err) = self.connect().await {
                        log::warn!(
                            "Failed to reconnect to MCP server '{}' after transport error: {}",
                            self.name,
                            reconnect_err,
                        );
                        // Emit Error notice about failed reconnect
                        self.emit_notice(
                            &format!("reconnect-failed:{}", self.name),
                            NoticeEvent::Error(format!(
                                "MCP server '{}' reconnection failed: {}",
                                self.name, reconnect_err
                            )),
                        );
                    }

                    // Return original transport error — do not retry since the
                    // tool call may have had side effects on the server
                    Err(anyhow::Error::from(err)).with_context(|| {
                        format!(
                            "MCP tool '{}' on '{}' failed due to transport error",
                            tool_name, self.name
                        )
                    })
                }
                other @ ServiceError::McpError(_) => {
                    Err(anyhow::Error::from(other)).with_context(|| {
                        format!(
                            "MCP tool '{}' on '{}' returned application error",
                            tool_name, self.name
                        )
                    })
                }
                other => Err(anyhow::Error::from(other)).with_context(|| {
                    format!("MCP tool '{}' on '{}' returned error", tool_name, self.name)
                }),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::handler::server::ServerHandler;
    use rmcp::model::{
        CallToolRequestParams, CallToolResult, ContentBlock, InitializeResult, ListToolsResult,
        ServerCapabilities, Tool,
    };
    use rmcp::service::{serve_server, NotificationContext, RoleServer};
    use serde_json::{json, Map};
    use std::time::Duration;
    use tokio::io::duplex;

    #[derive(Clone, Default, Debug)]
    struct MockServerHandler {
        initialized_params: Arc<RwLock<Option<InitializeRequestParams>>>,
        roots_list_changed_notified: Arc<RwLock<bool>>,
        peer: Arc<RwLock<Option<rmcp::service::Peer<rmcp::service::RoleServer>>>>,
        tools: Arc<RwLock<Vec<Tool>>>,
        last_tool_call: Arc<RwLock<Option<(String, Value)>>>,
    }

    impl ServerHandler for MockServerHandler {
        fn get_info(&self) -> InitializeResult {
            InitializeResult::new(ServerCapabilities::default())
                .with_server_info(Implementation::new("mock-server", "0.1.0"))
        }

        async fn initialize(
            &self,
            params: InitializeRequestParams,
            cx: RequestContext<rmcp::service::RoleServer>,
        ) -> Result<InitializeResult, rmcp::model::ErrorData> {
            *self.initialized_params.write() = Some(params);
            *self.peer.write() = Some(cx.peer.clone());
            Ok(self.get_info())
        }

        async fn list_tools(
            &self,
            _params: Option<rmcp::model::PaginatedRequestParams>,
            _cx: RequestContext<rmcp::service::RoleServer>,
        ) -> Result<ListToolsResult, rmcp::model::ErrorData> {
            Ok(ListToolsResult {
                meta: None,
                tools: self.tools.read().clone(),
                next_cursor: None,
            })
        }

        async fn call_tool(
            &self,
            request: CallToolRequestParams,
            _cx: RequestContext<RoleServer>,
        ) -> Result<CallToolResult, rmcp::model::ErrorData> {
            let arguments = request
                .arguments
                .clone()
                .map(Value::Object)
                .unwrap_or(Value::Null);
            *self.last_tool_call.write() = Some((request.name.to_string(), arguments.clone()));

            match request.name.as_ref() {
                "read" => {
                    let path = arguments
                        .get("path")
                        .and_then(Value::as_str)
                        .unwrap_or("<missing>");
                    Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                        "mock contents from {path}"
                    ))]))
                }
                other => Err(rmcp::model::ErrorData::invalid_params(
                    format!("unknown tool: {other}"),
                    None,
                )),
            }
        }

        async fn on_roots_list_changed(&self, _cx: NotificationContext<rmcp::service::RoleServer>) {
            *self.roots_list_changed_notified.write() = true;
        }
    }

    fn test_mcp_config(name: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            command: "mock-mcp".to_string(),
            args: vec![],
            env: HashMap::new(),
            roots: vec![],
            enabled: true,
            description: None,
            rename_tools: HashMap::new(),
            tool_templates: HashMap::new(),
            package: None,
            hooks: None,
        }
    }

    #[tokio::test]
    async fn test_mcp_roots_propagation() {
        let (client_transport, server_transport) = duplex(1024);

        let mock_server = MockServerHandler::default();
        let server_handler = mock_server.clone();

        // Client
        let roots = Arc::new(RwLock::new(vec!["/test/root".to_string()]));
        let handler = McpClientHandler::new(roots.clone());

        let server_fut = serve_server(server_handler, server_transport);
        let client_fut = rmcp::service::serve_client(handler, client_transport);

        let (_server_res, client_res) = tokio::join!(server_fut, client_fut);
        let _client_service = client_res.unwrap();
        let client_peer = _client_service.peer().clone();

        // Run client in background
        let _client_task = tokio::spawn(async move {
            let _ = _client_service.waiting().await;
        });

        // Verify roots in initialize params
        {
            let params = mock_server.initialized_params.read();
            let params = params.as_ref().unwrap();
            assert!(params.capabilities.roots.is_some());
            assert_eq!(
                params.capabilities.roots.as_ref().unwrap().list_changed,
                Some(true)
            );
        }

        let server_peer = mock_server.peer.read().as_ref().unwrap().clone();

        let roots_result = server_peer.list_roots().await.unwrap();
        assert_eq!(roots_result.roots.len(), 1);
        let uri = &roots_result.roots[0].uri;
        assert!(
            uri.starts_with("file:///") && uri.ends_with("/test/root"),
            "expected file:///...test/root, got: {uri}"
        );

        {
            roots.write().push("/test/root2".to_string());
            client_peer.notify_roots_list_changed().await.unwrap();
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(*mock_server.roots_list_changed_notified.read());

        let roots_result = server_peer.list_roots().await.unwrap();
        assert_eq!(roots_result.roots.len(), 2);
        let uri2 = &roots_result.roots[1].uri;
        assert!(
            uri2.starts_with("file:///") && uri2.ends_with("/test/root2"),
            "expected file:///...test/root2, got: {uri2}"
        );
    }

    #[tokio::test]
    async fn test_mcp_roots_canonicalization() {
        let (client_transport, server_transport) = duplex(1024);
        let mock_server = MockServerHandler::default();
        let server_handler = mock_server.clone();

        // Client with a relative root
        let roots = Arc::new(RwLock::new(vec![".".to_string()]));
        let handler = McpClientHandler::new(roots.clone());

        let server_fut = serve_server(server_handler, server_transport);
        let client_fut = rmcp::service::serve_client(handler, client_transport);

        let (_server_res, client_res) = tokio::join!(server_fut, client_fut);
        let _client_service = client_res.unwrap();

        // Run client in background
        let _client_task = tokio::spawn(async move {
            let _ = _client_service.waiting().await;
        });

        let server_peer = mock_server.peer.read().as_ref().unwrap().clone();

        // Verify list_roots works (server calling client)
        let roots_result = server_peer.list_roots().await.unwrap();
        assert_eq!(roots_result.roots.len(), 1);
        let uri = roots_result.roots[0].uri.clone();

        // It should be an absolute path
        assert!(
            uri.starts_with("file:///"),
            "URI should be an absolute file URI, got: {}",
            uri
        );

        let expected_path = std::env::current_dir().unwrap().canonicalize().unwrap();
        let expected_uri = path_to_file_uri(&expected_path);
        assert_eq!(uri, expected_uri);
    }

    #[test]
    fn test_mcp_roots_expansion() {
        let mut config = test_mcp_config("test");
        std::env::set_var("TEST_ROOT", "/tmp/test");
        config.roots = vec!["$TEST_ROOT/a".to_string(), "~/b".to_string()];

        let client = McpClient::new(config);
        let roots = client.get_roots();

        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0], "/tmp/test/a");
        let home = dirs::home_dir().unwrap();
        assert_eq!(roots[1], format!("{}/b", home.to_string_lossy()));
    }

    #[tokio::test]
    async fn test_mcp_roots_add_expansion() {
        let config = test_mcp_config("test");
        std::env::set_var("ADD_TEST_ROOT", "/tmp/add_test");
        let client = McpClient::new(config);

        client.add_root("$ADD_TEST_ROOT/c").await.unwrap();
        let roots = client.get_roots();

        assert!(roots.contains(&"/tmp/add_test/c".to_string()));
    }

    #[tokio::test]
    async fn test_mcp_manager_find_client_for_tool() {
        let manager = McpManager::new();
        let client = Arc::new(McpClient::new(test_mcp_config("fs")));
        *client.tools.write() = vec![
            mcp_tool_to_declaration(
                "fs_read",
                "read",
                "Read",
                &json!({}),
                ToolTemplates::default(),
                None,
            )
            .unwrap(),
            mcp_tool_to_declaration(
                "fs_write",
                "write",
                "Write",
                &json!({}),
                ToolTemplates::default(),
                None,
            )
            .unwrap(),
        ];
        manager.clients.write().insert("fs".to_string(), client);

        assert!(manager.find_client_for_tool("fs_read").is_some());
        assert!(manager.find_client_for_tool("fs_write").is_some());
        assert!(manager.find_client_for_tool("unknown_tool").is_none());
        assert!(manager.find_client_for_tool("noprefix").is_none());
    }

    /// Build a client pre-populated with `tools` (display names) and marked
    /// as already-failed so tool discovery does NOT try to spawn a real
    /// process for it. `get_tools_from_clients` skips connecting clients that
    /// have `connection_failed`, yet still returns their cached `get_tools()`,
    /// letting us exercise the selector-based server filter without I/O.
    fn preset_client(name: &str, tools: &[&str]) -> Arc<McpClient> {
        let client = Arc::new(McpClient::new(test_mcp_config(name)));
        let declarations = tools
            .iter()
            .map(|display| {
                let server_tool = display.strip_prefix(&format!("{name}_")).unwrap_or(display);
                mcp_tool_to_declaration(
                    display,
                    server_tool,
                    "desc",
                    &json!({}),
                    ToolTemplates::default(),
                    None,
                )
                .unwrap()
            })
            .collect();
        *client.tools.write() = declarations;
        *client.connection_failed.write() = true;
        client
    }

    fn tool_names(decls: &[ToolDeclaration]) -> Vec<String> {
        decls.iter().map(|d| d.name.clone()).collect()
    }

    #[tokio::test]
    async fn get_tools_for_selectors_filters_unmatched_servers() {
        let manager = McpManager::new();
        manager.clients.write().insert(
            "fs".to_string(),
            preset_client("fs", &["fs_read", "fs_write"]),
        );
        manager.clients.write().insert(
            "context7".to_string(),
            preset_client("context7", &["context7_search"]),
        );

        // Selector only references the `fs` server: context7 tools excluded.
        let tools = manager
            .get_tools_for_selectors(&["fs_read".to_string()])
            .await;
        assert_eq!(tool_names(&tools), vec!["fs_read", "fs_write"]);
        assert!(!tool_names(&tools).contains(&"context7_search".to_string()));
    }

    #[tokio::test]
    async fn get_tools_for_selectors_glob_selects_matching_server_only() {
        let manager = McpManager::new();
        manager.clients.write().insert(
            "fs".to_string(),
            preset_client("fs", &["fs_read", "fs_write"]),
        );
        manager.clients.write().insert(
            "context7".to_string(),
            preset_client("context7", &["context7_search"]),
        );

        let tools = manager
            .get_tools_for_selectors(&["context7_*".to_string()])
            .await;
        assert_eq!(tool_names(&tools), vec!["context7_search"]);
    }

    #[tokio::test]
    async fn get_tools_for_selectors_star_includes_all_servers() {
        let manager = McpManager::new();
        manager
            .clients
            .write()
            .insert("fs".to_string(), preset_client("fs", &["fs_read"]));
        manager.clients.write().insert(
            "context7".to_string(),
            preset_client("context7", &["context7_search"]),
        );

        let tools = manager.get_tools_for_selectors(&["*".to_string()]).await;
        assert_eq!(tool_names(&tools), vec!["context7_search", "fs_read"]);
    }

    #[tokio::test]
    async fn get_tools_for_selectors_includes_renamed_tool_server() {
        let manager = McpManager::new();
        let mut cfg = test_mcp_config("context7");
        cfg.rename_tools
            .insert("search".to_string(), "docs_search".to_string());
        let client = Arc::new(McpClient::new(cfg));
        *client.tools.write() = vec![mcp_tool_to_declaration(
            "docs_search",
            "search",
            "desc",
            &json!({}),
            ToolTemplates::default(),
            None,
        )
        .unwrap()];
        *client.connection_failed.write() = true;
        manager
            .clients
            .write()
            .insert("context7".to_string(), client);

        // The display name dropped the server prefix; selecting it by its
        // renamed name must still pick the context7 server.
        let tools = manager
            .get_tools_for_selectors(&["docs_search".to_string()])
            .await;
        assert_eq!(tool_names(&tools), vec!["docs_search"]);
    }

    #[tokio::test]
    async fn test_mcp_manager_call_tool_routes_prefixed_names() {
        let (client_transport, server_transport) = duplex(1024);
        let mock_server = MockServerHandler {
            tools: Arc::new(RwLock::new(vec![Tool::new(
                "read",
                "Read mock file contents.",
                Map::new(),
            )])),
            ..Default::default()
        };

        let server_handler = mock_server.clone();
        let client_handler = McpClientHandler::new(Arc::new(RwLock::new(vec![])));
        let (server_res, client_res) = tokio::join!(
            serve_server(server_handler, server_transport),
            rmcp::service::serve_client(client_handler, client_transport)
        );

        let _server_service = server_res.unwrap();
        let client_service = client_res.unwrap();

        let client = Arc::new(McpClient::new(test_mcp_config("fs")));
        *client.connected.write() = true;
        *client.tools.write() = vec![mcp_tool_to_declaration(
            "fs_read",
            "read",
            "Read mock file contents.",
            &json!({}),
            ToolTemplates::default(),
            None,
        )
        .unwrap()];
        *client.service.write() = Some(client_service);

        let manager = McpManager::new();
        manager.clients.write().insert("fs".to_string(), client);

        let result = manager
            .call_tool("fs_read", json!({ "path": "test.txt" }))
            .await
            .unwrap();

        let result_text = result.to_string();
        assert!(result_text.contains("mock contents from test.txt"));
        assert!(!result_text.contains("Unexpected call"));

        let last_tool_call = mock_server.last_tool_call.read().clone();
        assert_eq!(
            last_tool_call,
            Some((
                "read".to_string(),
                json!({
                    "path": "test.txt"
                }),
            ))
        );
    }

    #[test]
    fn test_template_merge_meta_only() {
        use crate::config::ToolDisplayTemplates;

        let meta_call = Some("meta call".to_string());
        let meta_result = Some("meta result".to_string());
        let cfg: Option<&ToolDisplayTemplates> = None;
        let call_template = cfg.and_then(|t| t.call_template.clone()).or(meta_call);
        let result_template = cfg.and_then(|t| t.result_template.clone()).or(meta_result);
        assert_eq!(call_template, Some("meta call".to_string()));
        assert_eq!(result_template, Some("meta result".to_string()));
    }

    #[test]
    fn test_template_merge_config_only() {
        use crate::config::ToolDisplayTemplates;

        let cfg_entry = ToolDisplayTemplates {
            call_template: Some("cfg call".to_string()),
            result_template: Some("cfg result".to_string()),
        };
        let cfg = Some(&cfg_entry);
        let call_template = cfg.and_then(|t| t.call_template.clone()).or(None);
        let result_template = cfg.and_then(|t| t.result_template.clone()).or(None);
        assert_eq!(call_template, Some("cfg call".to_string()));
        assert_eq!(result_template, Some("cfg result".to_string()));
    }

    #[test]
    fn test_template_merge_config_overrides_meta() {
        use crate::config::ToolDisplayTemplates;

        let meta_call = Some("meta call".to_string());
        let meta_result = Some("meta result".to_string());
        let cfg_entry = ToolDisplayTemplates {
            call_template: Some("cfg call".to_string()),
            result_template: Some("cfg result".to_string()),
        };
        let cfg = Some(&cfg_entry);
        let call_template = cfg.and_then(|t| t.call_template.clone()).or(meta_call);
        let result_template = cfg.and_then(|t| t.result_template.clone()).or(meta_result);
        assert_eq!(call_template, Some("cfg call".to_string()));
        assert_eq!(result_template, Some("cfg result".to_string()));
    }

    #[test]
    fn test_template_merge_partial_override() {
        use crate::config::ToolDisplayTemplates;

        let meta_call = Some("meta call".to_string());
        let meta_result = Some("meta result".to_string());
        let cfg_entry = ToolDisplayTemplates {
            call_template: Some("cfg call".to_string()),
            result_template: None,
        };
        let cfg = Some(&cfg_entry);
        let call_template = cfg.and_then(|t| t.call_template.clone()).or(meta_call);
        let result_template = cfg.and_then(|t| t.result_template.clone()).or(meta_result);
        assert_eq!(call_template, Some("cfg call".to_string()));
        assert_eq!(result_template, Some("meta result".to_string()));
    }

    #[test]
    fn test_render_stderr_tail_empty() {
        let buffer = new_stderr_buffer();
        assert_eq!(render_stderr_tail(&buffer), "");
    }

    #[test]
    fn test_render_stderr_tail_caps_lines() {
        let buffer = new_stderr_buffer();
        for i in 0..(MCP_STDERR_TAIL_LINES + 5) {
            let mut buf = buffer.lock();
            if buf.len() == MCP_STDERR_TAIL_LINES {
                buf.pop_front();
            }
            buf.push_back(format!("line-{i}"));
        }
        let rendered = render_stderr_tail(&buffer);
        assert!(rendered.starts_with("\nMCP server stderr:\n"));
        let body = rendered.trim_start_matches("\nMCP server stderr:\n");
        let lines: Vec<&str> = body.split('\n').collect();
        assert_eq!(lines.len(), MCP_STDERR_TAIL_LINES);
        assert_eq!(lines[0], "line-5");
        assert_eq!(
            lines[lines.len() - 1],
            format!("line-{}", MCP_STDERR_TAIL_LINES + 4)
        );
    }

    /// Spawning a fake "MCP server" (`sh -c "echo MARKER >&2; exit 1"`)
    /// fails initialization and the resulting error message must include
    /// the captured stderr line so users can diagnose bad-args failures
    /// (issue #391) instead of seeing a generic transport error.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_connect_includes_child_stderr_in_error() {
        let mut config = test_mcp_config("badserver");
        config.command = "sh".to_string();
        config.args = vec![
            "-c".to_string(),
            "echo 'unknown argument: --xyz' >&2; exit 1".to_string(),
        ];
        let client = McpClient::new(config);

        let err = client.connect().await.expect_err("expected connect error");
        let rendered = format!("{:#}", err);

        assert!(
            rendered.contains("badserver"),
            "error should mention server name; got: {rendered}"
        );
        assert!(
            rendered.contains("MCP server stderr:"),
            "error should include captured stderr header; got: {rendered}"
        );
        assert!(
            rendered.contains("unknown argument: --xyz"),
            "error should include actual stderr line; got: {rendered}"
        );
    }
}

/// Check whether a glob pattern matches a name. Returns `false` if the
/// pattern is invalid (graceful degradation). Mirrors the runtime's
/// `matches_tool_glob`; duplicated here to keep the dependency direction
/// (harnx-runtime depends on harnx-mcp, not vice versa).
fn matches_tool_glob(pattern: &str, name: &str) -> bool {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .ok()
        .is_some_and(|g| g.compile_matcher().is_match(name))
}

/// The leading literal (metachar-free) portion of a selector, i.e. the part
/// before the first glob metacharacter. For `"fs_*"` this is `"fs_"`, for
/// `"bash_exec"` it is the whole string, and for `"{a,b}"` it is `""`.
fn selector_literal_prefix(selector: &str) -> &str {
    selector
        .find(['*', '?', '{', '[', ']', '}'])
        .map_or(selector, |idx| &selector[..idx])
}

/// Whether any of `selectors` could match a tool contributed by `client`,
/// without connecting to it.
fn client_could_match_any_selector(client: &McpClient, selectors: &[String]) -> bool {
    let rename_targets: Vec<String> = client.config.rename_tools.values().cloned().collect();
    selectors.iter().any(|selector| {
        selector_could_match_server(selector.trim(), client.name(), &rename_targets)
    })
}

/// Decide whether a single `use_tools` selector could match any tool exposed
/// by the server named `server_name`. A server's tools are named
/// `{server_name}_{tool}` unless renamed, in which case the renamed display
/// names are listed in `rename_targets`.
///
/// This is a conservative pre-filter: it must never return `false` for a
/// selector that would actually select one of the server's tools (a false
/// negative would hide tools), but it may return `true` for a selector that
/// ends up matching nothing (a false positive only costs an extra
/// connection).
pub fn selector_could_match_server(
    selector: &str,
    server_name: &str,
    rename_targets: &[String],
) -> bool {
    if selector == "*" {
        return true;
    }

    // A renamed tool's display name no longer carries the server prefix, so
    // check those explicitly against the selector glob.
    if rename_targets
        .iter()
        .any(|target| matches_tool_glob(selector, target))
    {
        return true;
    }

    let server_prefix = format!("{server_name}_");

    // No glob metacharacters: the selector is an exact tool name and can only
    // belong to this server if it starts with the server's prefix.
    if !selector.contains(['*', '?', '{', '[', ']', '}']) {
        return selector.starts_with(&server_prefix);
    }

    // Glob selector: compare its literal leading segment against the server
    // prefix. The selector can only match a `{server}_…` tool if one prefix is
    // a prefix of the other (e.g. selector `fs_*` vs server `fs`, or selector
    // `f*` vs server `fs`). Selectors that begin with `*`/`{`/`[` have an
    // empty literal prefix and conservatively match every server.
    let literal_prefix = selector_literal_prefix(selector);
    server_prefix.starts_with(literal_prefix) || literal_prefix.starts_with(&server_prefix)
}

#[derive(Debug)]
pub struct McpManager {
    clients: Arc<RwLock<HashMap<String, Arc<McpClient>>>>,
}

impl McpManager {
    pub fn new() -> Self {
        Self {
            clients: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn get_client(&self, server_name: &str) -> Option<Arc<McpClient>> {
        self.clients.read().get(server_name).cloned()
    }

    /// Sorted list of registered (display) server names. For diagnostics.
    pub fn server_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.clients.read().keys().cloned().collect();
        names.sort();
        names
    }

    pub fn initialize(&self, configs: Vec<McpServerConfig>) {
        let mut clients = self.clients.write();
        clients.clear();

        for config in configs.into_iter().filter(|config| config.enabled) {
            clients.insert(config.name.clone(), Arc::new(McpClient::new(config)));
        }
    }

    pub async fn connect(&self, server_name: &str) -> Result<()> {
        let client = self
            .clients
            .read()
            .get(server_name)
            .cloned()
            .ok_or_else(|| anyhow!("Unknown MCP server '{}'", server_name))?;
        client.connect().await
    }

    pub async fn disconnect(&self, server_name: &str) -> Result<()> {
        let client = self
            .clients
            .read()
            .get(server_name)
            .cloned()
            .ok_or_else(|| anyhow!("Unknown MCP server '{}'", server_name))?;
        client.disconnect().await
    }

    /// Connect the given clients (best-effort, emitting a warning per
    /// connection failure) and collect the union of their tools, sorted by
    /// display name. Clients already connected or previously failed are not
    /// reconnected.
    async fn get_tools_from_clients(&self, clients: Vec<Arc<McpClient>>) -> Vec<ToolDeclaration> {
        let connect_futures: Vec<_> = clients
            .iter()
            .filter(|c| !c.is_connected() && !c.connection_failed())
            .map(|client| {
                let client = client.clone();
                async move {
                    if let Err(err) = client.connect().await {
                        // {:#} renders the full anyhow error chain so the
                        // MCP server's actual error (and any captured
                        // stderr tail) reaches the user, not just the
                        // outermost context.
                        let msg = format!(
                            "MCP server '{}' failed to connect: {:#}\nUse '.mcp connect {}' to retry.",
                            client.name(),
                            err,
                            client.name(),
                        );
                        let event = harnx_core::event::AgentEvent::Notice(
                            harnx_core::event::NoticeEvent::Warning(msg.clone()),
                        );
                        if !harnx_core::sink::emit_agent_event(event) {
                            eprintln!("Warning: {msg}");
                        }
                        log::warn!(
                            "MCP server '{}' connection failed: {:#}",
                            client.name(),
                            err,
                        );
                    }
                }
            })
            .collect();
        futures_util::future::join_all(connect_futures).await;

        let mut tools: Vec<_> = clients
            .iter()
            .flat_map(|client| client.get_tools())
            .collect();
        tools.sort_by(|left, right| left.name.cmp(&right.name));
        tools
    }

    pub async fn get_all_tools(&self) -> Vec<ToolDeclaration> {
        let clients: Vec<_> = self.clients.read().values().cloned().collect();
        self.get_tools_from_clients(clients).await
    }

    /// Like `get_all_tools`, but only connects to servers whose tools could
    /// match one of the given `use_tools` selectors. Servers that cannot
    /// contribute any matching tool are left untouched (never spawned), so
    /// agents that don't use a server's tools don't pay its startup cost or
    /// surface its connection errors (#790). Already-connected servers are
    /// always included.
    pub async fn get_tools_for_selectors(&self, selectors: &[String]) -> Vec<ToolDeclaration> {
        if selectors.iter().any(|s| s.trim() == "*") {
            return self.get_all_tools().await;
        }
        let clients: Vec<_> = self
            .clients
            .read()
            .values()
            .filter(|client| {
                client.is_connected() || client_could_match_any_selector(client, selectors)
            })
            .cloned()
            .collect();
        self.get_tools_from_clients(clients).await
    }

    /// Snapshot the names of every currently-connected MCP client.
    ///
    /// Used to distinguish services that were already warm *before* a
    /// short-lived-runtime discovery pass from those the discovery pass itself
    /// opened. See [`Self::invalidate_services_connected_since`].
    fn connected_client_names(&self) -> HashSet<String> {
        self.clients
            .read()
            .values()
            .filter(|client| client.is_connected())
            .map(|client| client.name().to_string())
            .collect()
    }

    /// Invalidate only services that became connected *after* the given
    /// snapshot — i.e. connections opened by a discovery pass running on a
    /// short-lived runtime, which would otherwise be left bound to a runtime
    /// that is about to be dropped.
    ///
    /// Services that were already connected before the snapshot (warm
    /// subprocesses established on the caller's persistent runtime by real
    /// tool calls) are preserved. Blanket-invalidating those was the cause of
    /// per-tool-round MCP subprocess churn on the single-threaded ACP server
    /// runtime (#988): each completion request reads the cached tool list via
    /// blocking discovery, and the old blanket invalidation tore down every
    /// live connection as a side effect, forcing a respawn on the next call.
    fn invalidate_services_connected_since(&self, previously_connected: &HashSet<String>) {
        for client in self.clients.read().values() {
            if client.is_connected() && !previously_connected.contains(client.name()) {
                client.invalidate_service();
            }
        }
    }

    /// Synchronously run an async tool-discovery future to completion,
    /// handling the various Tokio runtime contexts (multi-thread, current-
    /// thread, or none).
    ///
    /// On the current-thread and no-runtime paths the discovery future runs on
    /// a short-lived runtime; only services *opened by that discovery pass*
    /// are invalidated afterwards (they'd otherwise be orphaned when the
    /// short-lived runtime is dropped), while services that were already warm
    /// beforehand are preserved and re-used. On the multi-thread path the
    /// future runs on the caller's persistent runtime, so connections opened
    /// during discovery stay usable and nothing is invalidated. Either way,
    /// already-connected MCP subprocesses survive across discovery passes —
    /// avoiding the per-tool-round churn described in #988.
    fn run_tool_discovery_blocking<Fut>(
        &self,
        fut: impl FnOnce() -> Fut + Send,
    ) -> Vec<ToolDeclaration>
    where
        Fut: std::future::Future<Output = Vec<ToolDeclaration>>,
    {
        if let Ok(handle) = Handle::try_current() {
            match handle.runtime_flavor() {
                RuntimeFlavor::MultiThread => {
                    tokio::task::block_in_place(|| handle.block_on(fut()))
                }
                _ => {
                    // On a single-threaded runtime (e.g., the ACP server),
                    // block_in_place panics. Run the async operation on a
                    // dedicated thread with its own runtime instead.
                    let previously_connected = self.connected_client_names();
                    std::thread::scope(|s| {
                        s.spawn(|| {
                            let rt = Builder::new_current_thread()
                                .enable_all()
                                .build()
                                .expect("create runtime for MCP tool discovery");
                            let tools = rt.block_on(fut());
                            self.invalidate_services_connected_since(&previously_connected);
                            tools
                        })
                        .join()
                        .expect("MCP tool discovery thread panicked")
                    })
                }
            }
        } else {
            match Builder::new_current_thread().enable_all().build() {
                Ok(runtime) => {
                    let previously_connected = self.connected_client_names();
                    let tools = runtime.block_on(fut());
                    self.invalidate_services_connected_since(&previously_connected);
                    tools
                }
                Err(err) => {
                    log::warn!("Failed to create Tokio runtime for MCP tool discovery: {err}");
                    vec![]
                }
            }
        }
    }

    pub fn get_all_tools_blocking(&self) -> Vec<ToolDeclaration> {
        self.run_tool_discovery_blocking(|| self.get_all_tools())
    }

    /// Blocking variant of `get_tools_for_selectors`.
    pub fn get_tools_for_selectors_blocking(&self, selectors: &[String]) -> Vec<ToolDeclaration> {
        self.run_tool_discovery_blocking(|| self.get_tools_for_selectors(selectors))
    }

    pub async fn get_server_tools(&self, server_name: &str) -> Result<Vec<ToolDeclaration>> {
        let client = self
            .clients
            .read()
            .get(server_name)
            .cloned()
            .ok_or_else(|| anyhow!("Unknown MCP server '{}'", server_name))?;

        if !client.is_connected() {
            client.connect().await?;
        }

        Ok(client.get_tools())
    }

    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value> {
        let (client, server_tool_name) = self
            .find_client_for_tool(name)
            .ok_or_else(|| anyhow!("Unknown MCP tool '{}'", name))?;

        if !client.is_connected() {
            client.connect().await?;
        }

        client.call_tool(&server_tool_name, arguments).await
    }

    fn find_client_for_tool(&self, name: &str) -> Option<(Arc<McpClient>, String)> {
        for client in self.clients.read().values() {
            for tool in client.tools.read().iter() {
                if tool.name == name {
                    if let Some(ref server_tool_name) = tool.mcp_tool_name {
                        return Some((client.clone(), server_tool_name.clone()));
                    }
                }
            }
        }
        None
    }

    pub fn list_servers(&self) -> Vec<String> {
        let mut servers: Vec<_> = self
            .clients
            .read()
            .values()
            .map(|client| client.name().to_string())
            .collect();
        servers.sort();
        servers
    }
}

impl Default for McpManager {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ToolProvider for McpManager {
    fn name(&self) -> &str {
        "mcp"
    }

    fn has_tool(&self, tool_name: &str) -> bool {
        self.find_client_for_tool(tool_name).is_some()
    }

    async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Value,
        abort: &AbortSignal,
    ) -> Result<Value, ToolError> {
        tokio::select! {
            result = McpManager::call_tool(self, tool_name, arguments) => {
                result.map_err(ToolError::Recoverable)
            }
            _ = wait_abort_signal(abort) => {
                Err(ToolError::Fatal(anyhow!("MCP tool call aborted by user")))
            }
        }
    }
}

#[cfg(test)]
mod selector_filter_tests {
    use super::{classify_exit, selector_could_match_server, McpManager};
    use crate::McpServerConfig;
    use harnx_core::event::NoticeEvent;

    #[test]
    fn star_matches_every_server() {
        assert!(selector_could_match_server("*", "fs", &[]));
        assert!(selector_could_match_server("*", "context7", &[]));
    }

    #[test]
    fn exact_name_under_prefix_matches() {
        assert!(selector_could_match_server("fs_read", "fs", &[]));
        assert!(selector_could_match_server("bash_exec", "bash", &[]));
    }

    #[test]
    fn exact_name_for_other_server_does_not_match() {
        assert!(!selector_could_match_server("bash_exec", "fs", &[]));
        assert!(!selector_could_match_server("fetch_get", "context7", &[]));
    }

    #[test]
    fn server_glob_matches() {
        assert!(selector_could_match_server("fs_*", "fs", &[]));
        assert!(selector_could_match_server("context7_*", "context7", &[]));
    }

    #[test]
    fn other_server_glob_does_not_match() {
        assert!(!selector_could_match_server("fs_*", "context7", &[]));
        assert!(!selector_could_match_server("other_*", "fs", &[]));
    }

    #[test]
    fn partial_literal_prefix_glob_matches() {
        // Selector `f*` should conservatively match server `fs`.
        assert!(selector_could_match_server("f*", "fs", &[]));
        // But `x*` should not match `fs`.
        assert!(!selector_could_match_server("x*", "fs", &[]));
    }

    #[test]
    fn leading_metachar_glob_matches_conservatively() {
        // Empty literal prefix → matches any server (false positive allowed).
        assert!(selector_could_match_server("*_read", "fs", &[]));
        assert!(selector_could_match_server(
            "{fs_read,bash_exec}",
            "fs",
            &[]
        ));
        assert!(selector_could_match_server(
            "{fs_read,bash_exec}",
            "bash",
            &[]
        ));
    }

    #[test]
    fn renamed_tool_display_name_matches() {
        // A tool renamed to `docs_search` drops the server prefix; the
        // server must still be selected by an exact or glob selector.
        let targets = vec!["docs_search".to_string()];
        assert!(selector_could_match_server(
            "docs_search",
            "context7",
            &targets
        ));
        assert!(selector_could_match_server("docs_*", "context7", &targets));
    }

    #[test]
    fn renamed_tool_non_matching_selector_does_not_match() {
        let targets = vec!["docs_search".to_string()];
        assert!(!selector_could_match_server(
            "other_tool",
            "context7",
            &targets
        ));
    }

    // --- Part B tests: exit classification ---

    #[test]
    fn test_classify_exit_clean_exit_code_zero() {
        let event = classify_exit("test-server", Some(0), None);
        match event {
            NoticeEvent::Warning(msg) => {
                assert!(msg.contains("exited cleanly"));
            }
            _ => panic!("expected Warning for clean exit"),
        }
    }

    fn mock_mcp_bin() -> std::path::PathBuf {
        let exe_name = format!("harnx-mock-mcp{}", std::env::consts::EXE_SUFFIX);
        let current_exe = std::env::current_exe().expect("current test binary path");
        let target_dir = current_exe
            .parent()
            .expect("deps dir")
            .parent()
            .expect("target profile dir");
        let candidate = target_dir.join(&exe_name);
        assert!(
            candidate.exists(),
            "expected mock MCP binary at {}",
            candidate.display()
        );
        candidate
    }

    fn spawn_log_lines(path: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .map(|contents| {
                contents
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn wait_for_spawn_count(path: &std::path::Path, min_lines: usize) -> Vec<String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let lines = spawn_log_lines(path);
            if lines.len() >= min_lines {
                return lines;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {} spawn-log lines in {}. current contents: {:?}",
                min_lines,
                path.display(),
                lines
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mcp_subprocess_survives_repeated_discovery_on_current_thread() {
        let spawn_log = tempfile::NamedTempFile::new().expect("spawn log temp file");
        let spawn_log_path = spawn_log.path().to_path_buf();
        let manager = McpManager::new();
        manager.initialize(vec![McpServerConfig {
            name: "mock".to_string(),
            command: mock_mcp_bin().to_string_lossy().into_owned(),
            args: vec![
                "--spawn-log".to_string(),
                spawn_log_path.to_string_lossy().into_owned(),
            ],
            env: Default::default(),
            roots: vec![],
            enabled: true,
            description: None,
            rename_tools: Default::default(),
            tool_templates: Default::default(),
            hooks: None,
            package: None,
        }]);

        let tools = manager.get_all_tools_blocking();
        let echo_tool_name = tools
            .iter()
            .find(|tool| tool.name == "mock_echo")
            .map(|tool| tool.name.clone())
            .unwrap_or_else(|| panic!("expected mock_echo in tool list: {:?}", tools));

        manager
            .call_tool(&echo_tool_name, serde_json::json!({"text": "hi"}))
            .await
            .expect("initial mock_echo call should connect MCP subprocess");
        let first_lines = wait_for_spawn_count(&spawn_log_path, 1);
        let n1 = first_lines.len();

        for round in 0..3 {
            let rediscovered = manager.get_all_tools_blocking();
            assert!(
                rediscovered.iter().any(|tool| tool.name == echo_tool_name),
                "round {} rediscovery lost mock_echo: {:?}",
                round + 1,
                rediscovered
            );
            manager
                .call_tool(
                    &echo_tool_name,
                    serde_json::json!({"text": format!("hi-{round}")}),
                )
                .await
                .expect("repeated mock_echo call should reuse warm MCP subprocess");
        }

        let final_lines = spawn_log_lines(&spawn_log_path);
        let n2 = final_lines.len();
        eprintln!(
            "mcp_subprocess_survives_repeated_discovery_on_current_thread: n1={n1}, n2={n2}, log={:?}",
            final_lines
        );
        assert_eq!(
            n2, n1,
            "expected warm MCP subprocess to survive repeated discovery on current-thread runtime; n1={n1}, n2={n2}, log={:?}",
            final_lines
        );
    }

    #[test]
    fn test_classify_exit_nonzero_code() {
        let event = classify_exit("test-server", Some(3), None);
        match event {
            NoticeEvent::Error(msg) => {
                assert!(msg.contains("exited with code 3"));
            }
            _ => panic!("expected Error for nonzero exit code"),
        }
    }

    #[test]
    #[cfg(unix)]
    fn test_classify_exit_sigterm_warning() {
        let event = classify_exit("test-server", None, Some(15));
        match event {
            NoticeEvent::Warning(msg) => {
                assert!(msg.contains("terminated by SIGTERM"));
            }
            _ => panic!("expected Warning for SIGTERM"),
        }
    }

    #[test]
    #[cfg(unix)]
    fn test_classify_exit_sigint_warning() {
        let event = classify_exit("test-server", None, Some(2));
        match event {
            NoticeEvent::Warning(msg) => {
                assert!(msg.contains("terminated by SIGINT"));
            }
            _ => panic!("expected Warning for SIGINT"),
        }
    }

    #[test]
    #[cfg(unix)]
    fn test_classify_exit_sigkill_error() {
        let event = classify_exit("test-server", None, Some(9));
        match event {
            NoticeEvent::Error(msg) => {
                assert!(msg.contains("killed by SIGKILL"));
            }
            _ => panic!("expected Error for SIGKILL"),
        }
    }

    #[test]
    #[cfg(unix)]
    fn test_classify_exit_unknown_signal_error() {
        let event = classify_exit("test-server", None, Some(11)); // SIGSEGV
        match event {
            NoticeEvent::Error(msg) => {
                assert!(msg.contains("died: signal 11"));
            }
            _ => panic!("expected Error for unknown signal"),
        }
    }

    #[test]
    fn test_classify_exit_no_status_error() {
        let event = classify_exit("test-server", None, None);
        match event {
            NoticeEvent::Error(msg) => {
                assert!(msg.contains("status unavailable"));
            }
            _ => panic!("expected Error for no status"),
        }
    }
}

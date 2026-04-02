use harnx::mcp_safety::{format_size, sanitize_output_text, truncate_output, TruncateOpts};

use rmcp::model::{
    CallToolRequestParam, CallToolResult, Content, ErrorData, Implementation,
    ListToolsResult, PaginatedRequestParam, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::schemars::{generate::SchemaGenerator, JsonSchema, Schema};
use rmcp::service::{NotificationContext, RequestContext, RoleServer};
use rmcp::ServerHandler;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{Map, Value};
use std::borrow::Cow;
use std::fmt::Write as _;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::RwLock;

#[derive(Debug, Deserialize)]
struct ExecCommandParams {
    command: String,
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    head_lines: Option<usize>,
    #[serde(default)]
    tail_lines: Option<usize>,
    #[serde(default)]
    max_output_bytes: Option<usize>,
}

impl JsonSchema for ExecCommandParams {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("ExecCommandParams")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let command = generator.subschema_for::<String>();
        let working_dir = generator.subschema_for::<Option<String>>();
        let timeout_secs = generator.subschema_for::<Option<u64>>();
        let head_lines = generator.subschema_for::<Option<usize>>();
        let tail_lines = generator.subschema_for::<Option<usize>>();
        let max_output_bytes = generator.subschema_for::<Option<usize>>();
        object_schema(
            vec![
                ("command", command),
                ("working_dir", working_dir),
                ("timeout_secs", timeout_secs),
                ("head_lines", head_lines),
                ("tail_lines", tail_lines),
                ("max_output_bytes", max_output_bytes),
            ],
            &["command"],
        )
    }
}

#[derive(Clone)]
pub struct BashServer {
    roots: Arc<RwLock<Vec<PathBuf>>>,
    roots_initialized: Arc<AtomicBool>,
}

impl BashServer {
    pub fn new(initial_roots: Vec<PathBuf>) -> Self {
        Self {
            roots: Arc::new(RwLock::new(initial_roots)),
            roots_initialized: Arc::new(AtomicBool::new(false)),
        }
    }

    async fn refresh_roots(
        &self,
        peer: &rmcp::service::Peer<RoleServer>,
    ) -> Result<(), ErrorData> {
        let result = peer.list_roots().await.map_err(|err| {
            ErrorData::internal_error(
                format!("failed to fetch roots from peer: {err}"),
                None,
            )
        })?;

        let roots = result
            .roots
            .into_iter()
            .map(|root| root_uri_to_path(root.uri.as_ref()))
            .collect::<Vec<_>>();

        let mut guard = self.roots.write().await;
        *guard = roots;
        self.roots_initialized.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn ensure_roots_initialized(
        &self,
        peer: &rmcp::service::Peer<RoleServer>,
    ) -> Result<(), ErrorData> {
        if self.roots_initialized.load(Ordering::SeqCst) {
            return Ok(());
        }

        match self.refresh_roots(peer).await {
            Ok(()) => Ok(()),
            Err(err) => {
                if self.roots.read().await.is_empty() {
                    Err(err)
                } else {
                    Ok(())
                }
            }
        }
    }

    async fn exec_command_impl(
        &self,
        params: ExecCommandParams,
    ) -> Result<CallToolResult, ErrorData> {
        if params.command.trim().is_empty() {
            return Err(ErrorData::invalid_params("command cannot be empty", None));
        }

        let working_dir = self.resolve_working_dir(params.working_dir.as_deref()).await?;
        let timeout_secs = params.timeout_secs.unwrap_or(120);
        let default_opts = TruncateOpts::default();
        let truncate_opts = TruncateOpts {
            head_lines: params.head_lines.unwrap_or(default_opts.head_lines),
            tail_lines: params.tail_lines.unwrap_or(default_opts.tail_lines),
            line_head_bytes: default_opts.line_head_bytes,
            line_tail_bytes: default_opts.line_tail_bytes,
            max_output_bytes: params.max_output_bytes.unwrap_or(default_opts.max_output_bytes),
        };

        let mut child = Command::new("bash")
            .args(["-c", &params.command])
            .current_dir(&working_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| internal_error(format!("failed to spawn command: {err}")))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| internal_error("failed to capture stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| internal_error("failed to capture stderr"))?;

        let stdout_task = tokio::spawn(read_pipe(stdout));
        let stderr_task = tokio::spawn(read_pipe(stderr));

        let timeout = Duration::from_secs(timeout_secs);
        let timed_out = match tokio::time::timeout(timeout, child.wait()).await {
            Ok(Ok(status)) => Some(status),
            Ok(Err(err)) => {
                return Err(internal_error(format!("failed waiting for command: {err}")));
            }
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                None
            }
        };

        let stdout_bytes = join_pipe(stdout_task, "stdout").await?;
        let stderr_bytes = join_pipe(stderr_task, "stderr").await?;
        let output_bytes = merge_output(stdout_bytes, stderr_bytes);
        let total_bytes = output_bytes.len();
        let sanitized_output = sanitize_output_text(&String::from_utf8_lossy(&output_bytes));
        let total_lines = count_lines(&sanitized_output);
        let truncated_output = truncate_output(&sanitized_output, &truncate_opts);

        match timed_out {
            Some(status) => {
                let exit_code = status.code().unwrap_or(-1);
                let mut output = String::new();
                let _ = writeln!(output, "exit_code: {exit_code}");
                let _ = writeln!(output, "working_dir: {}", working_dir.display());
                let _ = writeln!(output, "total_lines: {total_lines}");
                let _ = writeln!(output, "total_bytes: {total_bytes} ({})", format_size(total_bytes));
                let _ = write!(output, "\n{}", render_output_block(&sanitized_output, &truncated_output));
                Ok(CallToolResult::success(vec![Content::text(output)]))
            }
            None => tool_error(render_timeout_message(
                &working_dir,
                timeout_secs,
                total_lines,
                total_bytes,
                &sanitized_output,
                &truncated_output,
            )),
        }
    }

    async fn resolve_working_dir(&self, requested: Option<&str>) -> Result<PathBuf, ErrorData> {
        let roots = self.roots.read().await;
        let default_dir = roots
            .first()
            .cloned()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        let resolved = match requested {
            Some(path_str) if !path_str.trim().is_empty() => {
                let path = PathBuf::from(path_str);
                if path.is_absolute() {
                    path
                } else {
                    default_dir.join(path)
                }
            }
            _ => default_dir,
        };

        let canonical = resolved.canonicalize().map_err(|err| {
            ErrorData::invalid_params(
                format!("cannot resolve working directory '{}': {err}", resolved.display()),
                None,
            )
        })?;

        if !canonical.is_dir() {
            return Err(ErrorData::invalid_params(
                format!("working directory '{}' is not a directory", canonical.display()),
                None,
            ));
        }

        if !roots.is_empty()
            && !roots.iter().any(|root| {
                root.canonicalize()
                    .map(|canonical_root| canonical.starts_with(&canonical_root))
                    .unwrap_or(false)
            })
        {
            return Err(ErrorData::invalid_params(
                format!("working directory '{}' is outside allowed roots", canonical.display()),
                None,
            ));
        }

        Ok(canonical)
    }
}

impl ServerHandler for BashServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: Default::default(),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "harnx-mcp-bash".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                title: None,
                website_url: None,
                icons: None,
            },
            instructions: Some("Local shell command MCP server with output truncation.".to_string()),
        }
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParam>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, ErrorData>> + Send + '_ {
        async move {
            Ok(ListToolsResult {
                tools: vec![Tool::new(
                    "exec_command",
                    "Execute a local shell command in a validated working directory and return truncated combined output.",
                    Map::new(),
                )
                .with_input_schema::<ExecCommandParams>()],
                next_cursor: None,
            })
        }
    }

    fn call_tool(
        &self,
        request: CallToolRequestParam,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResult, ErrorData>> + Send + '_ {
        async move {
            let _ = self.ensure_roots_initialized(&context.peer).await;

            match request.name.as_ref() {
                "exec_command" => {
                    let params = parse_arguments::<ExecCommandParams>(request.arguments)?;
                    self.exec_command_impl(params).await
                }
                other => Err(ErrorData::invalid_params(
                    format!("unknown tool: {other}"),
                    None,
                )),
            }
        }
    }

    fn on_roots_list_changed(
        &self,
        context: NotificationContext<RoleServer>,
    ) -> impl Future<Output = ()> + Send + '_ {
        async move {
            if let Err(err) = self.refresh_roots(&context.peer).await {
                eprintln!(
                    "harnx-mcp-bash: failed to refresh roots for {:?}: {}",
                    context.peer, err.message
                );
            }
        }
    }

    fn on_initialized(
        &self,
        context: NotificationContext<RoleServer>,
    ) -> impl Future<Output = ()> + Send + '_ {
        async move {
            if let Err(err) = self.refresh_roots(&context.peer).await {
                eprintln!(
                    "harnx-mcp-bash: failed to initialize roots for {:?}: {}",
                    context.peer, err.message
                );
            }
        }
    }
}

fn parse_arguments<T: DeserializeOwned>(
    arguments: Option<Map<String, Value>>,
) -> Result<T, ErrorData> {
    serde_json::from_value(Value::Object(arguments.unwrap_or_default())).map_err(|err| {
        ErrorData::invalid_params(format!("invalid tool arguments: {err}"), None)
    })
}

fn tool_error(msg: impl Into<String>) -> Result<CallToolResult, ErrorData> {
    Ok(CallToolResult::error(vec![Content::text(msg.into())]))
}

fn internal_error(msg: impl Into<Cow<'static, str>>) -> ErrorData {
    ErrorData::internal_error(msg, None)
}

fn root_uri_to_path(uri: &str) -> PathBuf {
    if let Some(path) = uri.strip_prefix("file://localhost") {
        return PathBuf::from(path);
    }
    if let Some(path) = uri.strip_prefix("file://") {
        return PathBuf::from(path);
    }
    PathBuf::from(uri)
}

fn object_schema(properties: Vec<(&str, Schema)>, required: &[&str]) -> Schema {
    let mut schema = Map::new();
    schema.insert("type".to_string(), Value::String("object".to_string()));

    let mut property_map = Map::new();
    for (name, property_schema) in properties {
        property_map.insert(name.to_string(), property_schema.as_value().clone());
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

async fn read_pipe<R>(mut reader: R) -> std::io::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).await?;
    Ok(bytes)
}

async fn join_pipe(
    task: tokio::task::JoinHandle<std::io::Result<Vec<u8>>>,
    name: &str,
) -> Result<Vec<u8>, ErrorData> {
    task.await
        .map_err(|err| internal_error(format!("failed to join {name} reader task: {err}")))?
        .map_err(|err| internal_error(format!("failed to read {name}: {err}")))
}

fn merge_output(stdout: Vec<u8>, stderr: Vec<u8>) -> Vec<u8> {
    if stdout.is_empty() {
        return stderr;
    }
    if stderr.is_empty() {
        return stdout;
    }

    let needs_separator = !stdout.ends_with(b"\n") && !stderr.starts_with(b"\n");
    let mut merged = Vec::with_capacity(stdout.len() + stderr.len() + usize::from(needs_separator));
    merged.extend_from_slice(&stdout);
    if needs_separator {
        merged.push(b'\n');
    }
    merged.extend_from_slice(&stderr);
    merged
}

fn count_lines(s: &str) -> usize {
    if s.is_empty() {
        0
    } else {
        s.lines().count()
    }
}

fn render_output_block(original: &str, truncated: &str) -> String {
    if truncated.is_empty() {
        return "output: <empty>".to_string();
    }

    if original == truncated {
        format!("output:\n{truncated}")
    } else {
        format!(
            "output:\n{truncated}\n\n[output truncated from {} to {}]",
            format_size(original.len()),
            format_size(truncated.len())
        )
    }
}

fn render_timeout_message(
    working_dir: &Path,
    timeout_secs: u64,
    total_lines: usize,
    total_bytes: usize,
    original: &str,
    truncated: &str,
) -> String {
    let mut output = String::new();
    let _ = writeln!(
        output,
        "command timed out after {timeout_secs}s and was terminated"
    );
    let _ = writeln!(output, "working_dir: {}", working_dir.display());
    let _ = writeln!(output, "total_lines: {total_lines}");
    let _ = writeln!(output, "total_bytes: {total_bytes} ({})", format_size(total_bytes));
    let _ = write!(output, "\n{}", render_output_block(original, truncated));
    output
}

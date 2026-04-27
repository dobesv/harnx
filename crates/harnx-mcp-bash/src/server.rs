use harnx_mcp::safety::{
    file_uri_to_path, format_size, sanitize_output_text, truncate_output, validate_path,
    TruncateOpts,
};

use fancy_regex::Regex;
use gix::ObjectId;
use harnx_mcp_history::HistoryManager;
#[cfg(windows)]
use process_wrap::tokio::JobObject;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, ErrorData, Implementation, ListToolsResult,
    Meta, PaginatedRequestParams, Role, ServerCapabilities, ServerInfo, Tool,
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

#[derive(Debug, Deserialize)]
struct ReadExecLogParams {
    execution_id: String,
    stream: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    tail: Option<usize>,
    #[serde(default)]
    grep: Option<String>,
    #[serde(default)]
    head_lines: Option<usize>,
    #[serde(default)]
    tail_lines: Option<usize>,
    #[serde(default)]
    max_output_bytes: Option<usize>,
}

impl JsonSchema for ReadExecLogParams {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("ReadExecLogParams")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let execution_id = generator.subschema_for::<String>();
        let stream = generator.subschema_for::<String>();
        let offset = generator.subschema_for::<Option<usize>>();
        let limit = generator.subschema_for::<Option<usize>>();
        let tail = generator.subschema_for::<Option<usize>>();
        let grep = generator.subschema_for::<Option<String>>();
        let head_lines = generator.subschema_for::<Option<usize>>();
        let tail_lines = generator.subschema_for::<Option<usize>>();
        let max_output_bytes = generator.subschema_for::<Option<usize>>();
        object_schema(
            vec![
                ("execution_id", execution_id),
                ("stream", stream),
                ("offset", offset),
                ("limit", limit),
                ("tail", tail),
                ("grep", grep),
                ("head_lines", head_lines),
                ("tail_lines", tail_lines),
                ("max_output_bytes", max_output_bytes),
            ],
            &["execution_id", "stream"],
        )
    }
}

#[derive(Debug, Deserialize)]
struct SpawnCommandParams {
    command: String,
    #[serde(default)]
    working_dir: Option<String>,
}

impl JsonSchema for SpawnCommandParams {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("SpawnCommandParams")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let command = generator.subschema_for::<String>();
        let working_dir = generator.subschema_for::<Option<String>>();
        object_schema(
            vec![("command", command), ("working_dir", working_dir)],
            &["command"],
        )
    }
}

#[derive(Debug, Deserialize)]
struct WaitParams {
    execution_id: String,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    head_lines: Option<usize>,
    #[serde(default)]
    tail_lines: Option<usize>,
    #[serde(default)]
    max_output_bytes: Option<usize>,
    #[serde(default)]
    grep: Option<String>,
}

impl JsonSchema for WaitParams {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("WaitParams")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let execution_id = generator.subschema_for::<String>();
        let timeout_secs = generator.subschema_for::<Option<u64>>();
        let head_lines = generator.subschema_for::<Option<usize>>();
        let tail_lines = generator.subschema_for::<Option<usize>>();
        let max_output_bytes = generator.subschema_for::<Option<usize>>();
        let grep = generator.subschema_for::<Option<String>>();
        object_schema(
            vec![
                ("execution_id", execution_id),
                ("timeout_secs", timeout_secs),
                ("head_lines", head_lines),
                ("tail_lines", tail_lines),
                ("max_output_bytes", max_output_bytes),
                ("grep", grep),
            ],
            &["execution_id"],
        )
    }
}

#[derive(Debug, Deserialize)]
struct TerminateParams {
    execution_id: String,
    #[serde(default)]
    signal: Option<String>,
}

impl JsonSchema for TerminateParams {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("TerminateParams")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let execution_id = generator.subschema_for::<String>();
        let signal = generator.subschema_for::<Option<String>>();
        object_schema(
            vec![("execution_id", execution_id), ("signal", signal)],
            &["execution_id"],
        )
    }
}

#[derive(Debug, Deserialize)]
struct RollbackParams {
    commit_id: String,
    repo_path: String,
}

impl JsonSchema for RollbackParams {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("RollbackParams")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let commit_id = generator.subschema_for::<String>();
        let repo_path = generator.subschema_for::<String>();
        object_schema(
            vec![("commit_id", commit_id), ("repo_path", repo_path)],
            &["commit_id", "repo_path"],
        )
    }
}

// ---------------------------------------------------------------------------
// Spawned process tracking
// ---------------------------------------------------------------------------

struct SpawnedProcess {
    child: Box<dyn ChildWrapper>,
    command: String,
    working_dir: PathBuf,
    stdout_log_path: PathBuf,
    stderr_log_path: PathBuf,
    before_snap_ids: Vec<(PathBuf, gix::ObjectId)>,
}

struct BashServerInner {
    roots: RwLock<Vec<PathBuf>>,
    roots_initialized: AtomicBool,
    spawned: Mutex<HashMap<String, SpawnedProcess>>,
    log_dir: PathBuf,
    history: Arc<HistoryManager>,
}

// ---------------------------------------------------------------------------
// BashServer
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct BashServer {
    inner: Arc<BashServerInner>,
}

impl BashServer {
    pub fn new(initial_roots: Vec<PathBuf>) -> Self {
        let log_dir = std::env::temp_dir().join(format!(
            "harnx-bash-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        Self {
            inner: Arc::new(BashServerInner {
                roots: RwLock::new(initial_roots.clone()),
                roots_initialized: AtomicBool::new(false),
                spawned: Mutex::new(HashMap::new()),
                log_dir,
                history: Arc::new(HistoryManager::new(&initial_roots)),
            }),
        }
    }

    async fn refresh_roots(&self, peer: &rmcp::service::Peer<RoleServer>) -> Result<(), ErrorData> {
        let result = peer.list_roots().await.map_err(|err| {
            ErrorData::internal_error(format!("failed to fetch roots from peer: {err}"), None)
        })?;

        let roots = result
            .roots
            .into_iter()
            .filter_map(|root| file_uri_to_path(root.uri.as_ref()))
            .collect::<Vec<_>>();

        let mut guard = self.inner.roots.write().await;
        *guard = roots;
        self.inner.roots_initialized.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn ensure_roots_initialized(
        &self,
        peer: &rmcp::service::Peer<RoleServer>,
    ) -> Result<(), ErrorData> {
        if self.inner.roots_initialized.load(Ordering::SeqCst) {
            return Ok(());
        }

        match self.refresh_roots(peer).await {
            Ok(()) => Ok(()),
            Err(err) => {
                if self.inner.roots.read().await.is_empty() {
                    Err(err)
                } else {
                    Ok(())
                }
            }
        }
    }

    async fn ensure_log_dir(&self) -> Result<(), ErrorData> {
        if let Some(parent) = self.inner.log_dir.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|err| {
                internal_error(format!(
                    "failed to create temp parent directory '{}': {err}",
                    parent.display()
                ))
            })?;
        }

        tokio::fs::create_dir_all(&self.inner.log_dir)
            .await
            .map_err(|err| {
                internal_error(format!(
                    "failed to create log directory '{}': {err}",
                    self.inner.log_dir.display()
                ))
            })
    }

    fn next_exec_dir(&self) -> Result<tempfile::TempDir, ErrorData> {
        tempfile::Builder::new()
            .prefix("exec-")
            .tempdir_in(&self.inner.log_dir)
            .map_err(|err| internal_error(format!("failed to create exec directory: {err}")))
    }

    // -----------------------------------------------------------------------
    // exec
    // -----------------------------------------------------------------------

    async fn exec_command_impl(
        &self,
        params: ExecCommandParams,
    ) -> Result<CallToolResult, ErrorData> {
        if params.command.trim().is_empty() {
            return Err(ErrorData::invalid_params("command cannot be empty", None));
        }

        let working_dir = self
            .resolve_working_dir(params.working_dir.as_deref())
            .await?;

        // HISTORY: before snapshot (non-fatal)
        let before_snaps = self
            .inner
            .history
            .snapshot_repos_for_dir(&working_dir, "before exec")
            .await
            .unwrap_or_else(|e| {
                log::warn!("history before-snapshot failed: {e}");
                vec![]
            });

        let timeout_secs = params.timeout_secs.unwrap_or(120);
        let default_opts = TruncateOpts::default();
        let truncate_opts = TruncateOpts {
            head_lines: params.head_lines.unwrap_or(default_opts.head_lines),
            tail_lines: params.tail_lines.unwrap_or(default_opts.tail_lines),
            line_head_bytes: default_opts.line_head_bytes,
            line_tail_bytes: default_opts.line_tail_bytes,
            max_output_bytes: params
                .max_output_bytes
                .unwrap_or(default_opts.max_output_bytes),
            ..default_opts
        };

        self.ensure_log_dir().await?;

        let exec_dir = self.next_exec_dir()?.keep();
        let stdout_log_path = exec_dir.join("stdout.log");
        let stderr_log_path = exec_dir.join("stderr.log");
        let execution_id = exec_dir.file_name().unwrap().to_string_lossy().into_owned();

        let stdout_file = tokio::fs::File::create(&stdout_log_path)
            .await
            .map_err(|err| {
                internal_error(format!(
                    "failed to create stdout log file '{}': {err}",
                    stdout_log_path.display()
                ))
            })?;
        let stderr_file = tokio::fs::File::create(&stderr_log_path)
            .await
            .map_err(|err| {
                internal_error(format!(
                    "failed to create stderr log file '{}': {err}",
                    stderr_log_path.display()
                ))
            })?;

        let mut command = CommandWrap::with_new("bash", |command| {
            command
                .args(["-c", &params.command])
                .current_dir(&working_dir)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
        });
        command.wrap(KillOnDrop);
        #[cfg(unix)]
        command.wrap(ProcessGroup::leader());
        #[cfg(windows)]
        command.wrap(JobObject);

        let mut child = command
            .spawn()
            .map_err(|err| internal_error(format!("failed to spawn command: {err}")))?;

        let stdout = child
            .stdout()
            .take()
            .ok_or_else(|| internal_error("failed to capture stdout"))?;
        let stderr = child
            .stderr()
            .take()
            .ok_or_else(|| internal_error("failed to capture stderr"))?;

        let stdout_task = tokio::spawn(read_pipe_to_file(stdout, stdout_file));
        let stderr_task = tokio::spawn(read_pipe_to_file(stderr, stderr_file));

        let timeout = Duration::from_secs(timeout_secs);
        let (status, timed_out) = match tokio::time::timeout(timeout, child.wait()).await {
            Ok(Ok(status)) => (Some(status), false),
            Ok(Err(err)) => {
                return Err(internal_error(format!("failed waiting for command: {err}")));
            }
            Err(_) => {
                child.start_kill().map_err(|err| {
                    internal_error(format!("failed to kill command after timeout: {err}"))
                })?;
                match child.wait().await {
                    Ok(status) => (Some(status), true),
                    Err(err) => {
                        return Err(internal_error(format!(
                            "failed waiting for killed command: {err}"
                        )));
                    }
                }
            }
        };

        let stdout_bytes = join_pipe(stdout_task, "stdout").await?;
        let stderr_bytes = join_pipe(stderr_task, "stderr").await?;

        // Sync log files to disk to ensure they're visible to other processes immediately
        if let Ok(f) = tokio::fs::File::open(&stdout_log_path).await {
            let _ = f.sync_all().await;
        }
        if let Ok(f) = tokio::fs::File::open(&stderr_log_path).await {
            let _ = f.sync_all().await;
        }

        let stdout_str = String::from_utf8_lossy(&stdout_bytes).into_owned();
        let stderr_str = String::from_utf8_lossy(&stderr_bytes).into_owned();
        // exec does not expose a grep param; pass None
        let (streams_block, stdout_lines, stderr_lines, stdout_bytes_len, stderr_bytes_len) =
            render_streams_block(
                &stdout_str,
                &stderr_str,
                &truncate_opts,
                None,
                &execution_id,
                &stdout_log_path,
                &stderr_log_path,
            );
        let total_lines = stdout_lines + stderr_lines;
        let total_bytes = stdout_bytes_len + stderr_bytes_len;

        match (status, timed_out) {
            (Some(status), false) => {
                let exit_code = status.code().unwrap_or(-1);
                let mut output = String::new();
                let _ = writeln!(output, "execution_id: {execution_id}");
                let _ = writeln!(output, "exit_code: {exit_code}");
                let _ = writeln!(output, "working_dir: {}", working_dir.display());
                let _ = writeln!(output, "stdout_log_path: {}", stdout_log_path.display());
                let _ = writeln!(output, "stderr_log_path: {}", stderr_log_path.display());
                let _ = writeln!(output, "total_lines: {total_lines}");
                let _ = writeln!(
                    output,
                    "total_bytes: {total_bytes} ({})",
                    format_size(total_bytes)
                );
                let _ = write!(output, "\n{streams_block}");
                let summary = format!(
                    "exit_code: {exit_code}, {total_lines} lines, {}",
                    format_size(total_bytes)
                );

                // HISTORY: after snapshot + diff (non-fatal)
                let mut diff_parts: Vec<String> = Vec::new();
                if !before_snaps.is_empty() {
                    let after_snaps = self
                        .inner
                        .history
                        .snapshot_repos_for_dir(&working_dir, "after exec")
                        .await
                        .unwrap_or_else(|e| {
                            log::warn!("history after-snapshot failed: {e}");
                            vec![]
                        });

                    for (repo_dir, before_id) in &before_snaps {
                        if let Some((_, after_id)) = after_snaps.iter().find(|(d, _)| d == repo_dir)
                        {
                            // Always emit snapshot ID
                            if before_id != after_id {
                                match self
                                    .inner
                                    .history
                                    .diff_commits(repo_dir, *before_id, *after_id)
                                    .await
                                {
                                    Ok(diff) if !diff.is_empty() => diff_parts.push(diff),
                                    Ok(_) => {}
                                    Err(e) => log::warn!("history diff failed: {e}"),
                                }
                            }
                        }
                    }
                }

                let mut contents = vec![
                    Content::text(output).with_audience(vec![Role::Assistant]),
                    Content::text(summary).with_audience(vec![Role::User]),
                ];
                for diff in diff_parts {
                    contents.push(Content::text(diff));
                }
                Ok(CallToolResult::success(contents))
            }
            (Some(status), true) => {
                let _ = status;
                tool_error(render_timeout_message(TimeoutRenderContext {
                    working_dir: &working_dir,
                    execution_id: &execution_id,
                    timeout_secs,
                    total_lines,
                    total_bytes,
                    stdout: &stdout_str,
                    stderr: &stderr_str,
                    truncate_opts: &truncate_opts,
                    stdout_log_path: &stdout_log_path,
                    stderr_log_path: &stderr_log_path,
                }))
            }
            (None, _) => tool_error("process exited without status".to_string()),
        }
    }

    // -----------------------------------------------------------------------
    // read_exec_log
    // -----------------------------------------------------------------------

    async fn read_exec_log_impl(
        &self,
        params: ReadExecLogParams,
    ) -> Result<CallToolResult, ErrorData> {
        if params.offset.is_some() && params.tail.is_some() {
            return Err(ErrorData::invalid_params(
                "offset and tail are mutually exclusive",
                None,
            ));
        }

        // Validate stream parameter
        if params.stream != "stdout" && params.stream != "stderr" {
            return Err(ErrorData::invalid_params(
                format!(
                    "stream must be 'stdout' or 'stderr', got '{}'",
                    params.stream
                ),
                None,
            ));
        }

        // Construct absolute path: log_dir/execution_id/stream.log
        // We pass the absolute path string so validate_path uses canonicalize() correctly
        // (passing a relative string would resolve against cwd, not log_dir).
        let abs = self
            .inner
            .log_dir
            .join(&params.execution_id)
            .join(format!("{}.log", params.stream));
        let path = validate_path(
            abs.to_string_lossy().as_ref(),
            std::slice::from_ref(&self.inner.log_dir),
        )
        .map_err(|err| {
            if err.starts_with("Cannot resolve path") {
                ErrorData::invalid_params(
                    format!(
                        "cannot resolve execution_id '{}': {}",
                        params.execution_id, err
                    ),
                    None,
                )
            } else {
                ErrorData::invalid_params(
                    format!(
                        "execution_id '{}' is outside the bash server temp log directory",
                        params.execution_id
                    ),
                    None,
                )
            }
        })?;
        let metadata = tokio::fs::metadata(&path)
            .await
            .map_err(|err| internal_error(format!("cannot access '{}': {err}", path.display())))?;

        if !metadata.is_file() {
            return tool_error(format!("'{}' is not a regular log file.", path.display()));
        }

        let raw_content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|err| internal_error(format!("failed to read '{}': {err}", path.display())))?;
        // Apply same sanitization as exec/wait output rendering so control characters
        // don't leak through the log-read path.
        let content = sanitize_output_text(&raw_content);

        let grep_regex = match params.grep.as_deref() {
            Some(pattern) => Some(Regex::new(pattern).map_err(|err| {
                ErrorData::invalid_params(format!("invalid grep pattern: {err}"), None)
            })?),
            None => None,
        };

        let mut notices = Vec::new();
        let mut regex_error = None;
        let mut numbered_lines = content
            .lines()
            .enumerate()
            .filter_map(|(idx, line)| {
                let line_number = idx + 1;
                match grep_regex.as_ref() {
                    Some(regex) => match regex.is_match(line) {
                        Ok(true) => Some((line_number, line.to_string())),
                        Ok(false) => None,
                        Err(err) => {
                            if regex_error.is_none() {
                                regex_error = Some(err.to_string());
                            }
                            None
                        }
                    },
                    None => Some((line_number, line.to_string())),
                }
            })
            .collect::<Vec<_>>();

        if let Some(err) = regex_error {
            notices.push(format!("grep evaluation error: {err}"));
        }

        let total_matching_lines = numbered_lines.len();
        if total_matching_lines == 0 {
            let mut output = String::from("<no matching lines>");
            if let Some(pattern) = params.grep.as_deref() {
                let _ = write!(output, "\n\n[no lines matched grep pattern '{}']", pattern);
            }
            let summary = format!("Read {} (0 lines)", path.display());
            return Ok(CallToolResult::success(vec![
                Content::text(output).with_audience(vec![Role::Assistant]),
                Content::text(summary).with_audience(vec![Role::User]),
            ]));
        }

        if let Some(tail) = params.tail {
            if tail < total_matching_lines {
                notices.push(format!(
                    "showing last {} of {} matching lines",
                    tail, total_matching_lines
                ));
                numbered_lines = numbered_lines[total_matching_lines - tail..].to_vec();
            }
        } else if let Some(offset) = params.offset {
            if offset == 0 {
                return Err(ErrorData::invalid_params("offset must be >= 1", None));
            }

            let limit = params.limit.unwrap_or(200).max(1);
            if offset > total_matching_lines {
                return tool_error(format!(
                    "offset {} is beyond the {} matching lines in {}",
                    offset,
                    total_matching_lines,
                    path.display()
                ));
            }

            let start = offset - 1;
            let end = (start + limit).min(total_matching_lines);
            if end < total_matching_lines {
                notices.push(format!(
                    "{} more matching lines. Use offset={} to continue",
                    total_matching_lines - end,
                    end + 1
                ));
            }
            numbered_lines = numbered_lines[start..end].to_vec();
        }

        let raw_output = numbered_lines
            .into_iter()
            .map(|(line_number, line)| format!("{line_number}: {line}"))
            .collect::<Vec<_>>()
            .join("\n");

        let default_opts = TruncateOpts::default();
        let truncate_opts = TruncateOpts {
            head_lines: params.head_lines.unwrap_or(default_opts.head_lines),
            tail_lines: params.tail_lines.unwrap_or(default_opts.tail_lines),
            line_head_bytes: default_opts.line_head_bytes,
            line_tail_bytes: default_opts.line_tail_bytes,
            max_output_bytes: params
                .max_output_bytes
                .unwrap_or(default_opts.max_output_bytes),
            ..default_opts
        };
        let truncated_output = truncate_output(&raw_output, &truncate_opts);

        if truncated_output != raw_output {
            notices.push(format!(
                "output truncated from {} to {}. Use head_lines, tail_lines, or max_output_bytes to see more",
                format_size(raw_output.len()),
                format_size(truncated_output.len())
            ));
        }

        let mut output = truncated_output;
        if !notices.is_empty() {
            let _ = write!(output, "\n\n[{}]", notices.join(". "));
        }

        let summary = format!(
            "Read {}/{} ({} lines, {})",
            params.execution_id,
            params.stream,
            total_matching_lines,
            format_size(raw_output.len())
        );
        Ok(CallToolResult::success(vec![
            Content::text(output).with_audience(vec![Role::Assistant]),
            Content::text(summary).with_audience(vec![Role::User]),
        ]))
    }

    // -----------------------------------------------------------------------
    // spawn
    // -----------------------------------------------------------------------

    async fn spawn_impl(&self, params: SpawnCommandParams) -> Result<CallToolResult, ErrorData> {
        if params.command.trim().is_empty() {
            return Err(ErrorData::invalid_params("command cannot be empty", None));
        }

        let working_dir = self
            .resolve_working_dir(params.working_dir.as_deref())
            .await?;

        // HISTORY: before snapshot (non-fatal)
        let before_snap_ids = self
            .inner
            .history
            .snapshot_repos_for_dir(&working_dir, "before spawn")
            .await
            .unwrap_or_else(|e| {
                log::warn!("history before-snapshot failed: {e}");
                vec![]
            });

        self.ensure_log_dir().await?;

        let exec_dir = self.next_exec_dir()?.keep();
        let stdout_log_path = exec_dir.join("stdout.log");
        let stderr_log_path = exec_dir.join("stderr.log");
        let execution_id = exec_dir.file_name().unwrap().to_string_lossy().into_owned();

        let stdout_file = std::fs::File::create(&stdout_log_path).map_err(|err| {
            internal_error(format!(
                "failed to create stdout log file '{}': {err}",
                stdout_log_path.display()
            ))
        })?;
        let stderr_file = std::fs::File::create(&stderr_log_path).map_err(|err| {
            internal_error(format!(
                "failed to create stderr log file '{}': {err}",
                stderr_log_path.display()
            ))
        })?;

        let mut command = CommandWrap::with_new("bash", |command| {
            command
                .args(["-c", &params.command])
                .current_dir(&working_dir)
                .stdin(Stdio::null())
                .stdout(Stdio::from(stdout_file))
                .stderr(Stdio::from(stderr_file));
        });
        #[cfg(unix)]
        command.wrap(ProcessGroup::leader());
        #[cfg(windows)]
        command.wrap(JobObject);

        let child = command
            .spawn()
            .map_err(|err| internal_error(format!("failed to spawn command: {err}")))?;

        let entry = SpawnedProcess {
            child,
            command: params.command.clone(),
            working_dir: working_dir.clone(),
            stdout_log_path: stdout_log_path.clone(),
            stderr_log_path: stderr_log_path.clone(),
            before_snap_ids,
        };

        self.inner
            .spawned
            .lock()
            .await
            .insert(execution_id.clone(), entry);

        let mut output = String::new();
        let _ = writeln!(output, "execution_id: {execution_id}");
        let _ = writeln!(output, "stdout_log_path: {}", stdout_log_path.display());
        let _ = writeln!(output, "stderr_log_path: {}", stderr_log_path.display());
        let _ = writeln!(output, "working_dir: {}", working_dir.display());
        let _ = write!(output, "command: {}", params.command);
        let summary = format!("spawned {execution_id}");

        Ok(CallToolResult::success(vec![
            Content::text(output).with_audience(vec![Role::Assistant]),
            Content::text(summary).with_audience(vec![Role::User]),
        ]))
    }

    // -----------------------------------------------------------------------
    // wait
    // -----------------------------------------------------------------------

    async fn wait_impl(&self, params: WaitParams) -> Result<CallToolResult, ErrorData> {
        let timeout_secs = params.timeout_secs.unwrap_or(120);
        let default_opts = TruncateOpts::default();
        let truncate_opts = TruncateOpts {
            head_lines: params.head_lines.unwrap_or(default_opts.head_lines),
            tail_lines: params.tail_lines.unwrap_or(default_opts.tail_lines),
            line_head_bytes: default_opts.line_head_bytes,
            line_tail_bytes: default_opts.line_tail_bytes,
            max_output_bytes: params
                .max_output_bytes
                .unwrap_or(default_opts.max_output_bytes),
            ..default_opts
        };

        let (mut child, command, working_dir, stdout_log_path, stderr_log_path, before_snap_ids) = {
            let mut map = self.inner.spawned.lock().await;
            let entry = map.remove(&params.execution_id).ok_or_else(|| {
                ErrorData::invalid_params(
                    format!(
                        "execution_id '{}' is not a tracked background process (or already waited on)",
                        params.execution_id
                    ),
                    None,
                )
            })?;
            (
                entry.child,
                entry.command,
                entry.working_dir,
                entry.stdout_log_path,
                entry.stderr_log_path,
                entry.before_snap_ids,
            )
        };

        let timeout = Duration::from_secs(timeout_secs);
        let wait_result = tokio::time::timeout(timeout, child.wait()).await;

        // Read both log files
        let stdout_content = tokio::fs::read_to_string(&stdout_log_path)
            .await
            .unwrap_or_default();
        let stderr_content = tokio::fs::read_to_string(&stderr_log_path)
            .await
            .unwrap_or_default();

        let grep_regex = match params.grep.as_deref() {
            Some(pattern) => Some(Regex::new(pattern).map_err(|err| {
                ErrorData::invalid_params(format!("invalid grep pattern: {err}"), None)
            })?),
            None => None,
        };
        let (streams_block, stdout_lines, stderr_lines, stdout_bytes_len, stderr_bytes_len) =
            render_streams_block(
                &stdout_content,
                &stderr_content,
                &truncate_opts,
                grep_regex.as_ref(),
                &params.execution_id,
                &stdout_log_path,
                &stderr_log_path,
            );
        let total_lines = stdout_lines + stderr_lines;
        let total_bytes = stdout_bytes_len + stderr_bytes_len;

        match wait_result {
            Ok(Ok(status)) => {
                let exit_code = status.code().unwrap_or(-1);
                let mut output = String::new();
                let _ = writeln!(output, "execution_id: {}", params.execution_id);
                let _ = writeln!(output, "status: exited");
                let _ = writeln!(output, "exit_code: {exit_code}");
                let _ = writeln!(output, "working_dir: {}", working_dir.display());
                let _ = writeln!(output, "command: {command}");
                let _ = writeln!(output, "stdout_log_path: {}", stdout_log_path.display());
                let _ = writeln!(output, "stderr_log_path: {}", stderr_log_path.display());
                let _ = writeln!(output, "total_lines: {total_lines}");
                let _ = writeln!(
                    output,
                    "total_bytes: {total_bytes} ({})",
                    format_size(total_bytes)
                );
                let _ = write!(output, "\n{streams_block}");
                let summary = format!(
                    "execution_id '{}' exited with code {exit_code}",
                    params.execution_id
                );

                // HISTORY: after snapshot + diff (non-fatal)
                let mut diff_parts: Vec<String> = Vec::new();
                if !before_snap_ids.is_empty() {
                    let after_snaps = self
                        .inner
                        .history
                        .snapshot_repos_for_dir(&working_dir, "after wait")
                        .await
                        .unwrap_or_else(|e| {
                            log::warn!("history after-snapshot failed: {e}");
                            vec![]
                        });
                    for (repo_dir, before_id) in &before_snap_ids {
                        if let Some((_, after_id)) = after_snaps.iter().find(|(d, _)| d == repo_dir)
                        {
                            // Always emit snapshot ID
                            if before_id != after_id {
                                match self
                                    .inner
                                    .history
                                    .diff_commits(repo_dir, *before_id, *after_id)
                                    .await
                                {
                                    Ok(diff) if !diff.is_empty() => diff_parts.push(diff),
                                    Ok(_) => {}
                                    Err(e) => log::warn!("history diff failed: {e}"),
                                }
                            }
                        }
                    }
                }

                let mut contents = vec![
                    Content::text(output).with_audience(vec![Role::Assistant]),
                    Content::text(summary).with_audience(vec![Role::User]),
                ];
                for diff in diff_parts {
                    contents.push(Content::text(diff));
                }
                Ok(CallToolResult::success(contents))
            }
            Ok(Err(err)) => Err(internal_error(format!(
                "failed waiting for execution_id '{}': {err}",
                params.execution_id
            ))),
            Err(_) => {
                let mut map = self.inner.spawned.lock().await;
                map.insert(
                    params.execution_id.clone(),
                    SpawnedProcess {
                        child,
                        command: command.clone(),
                        working_dir: working_dir.clone(),
                        stdout_log_path: stdout_log_path.clone(),
                        stderr_log_path: stderr_log_path.clone(),
                        before_snap_ids,
                    },
                );

                let mut output = String::new();
                let _ = writeln!(output, "execution_id: {}", params.execution_id);
                let _ = writeln!(output, "status: running");
                let _ = writeln!(output, "working_dir: {}", working_dir.display());
                let _ = writeln!(output, "command: {command}");
                let _ = writeln!(output, "stdout_log_path: {}", stdout_log_path.display());
                let _ = writeln!(output, "stderr_log_path: {}", stderr_log_path.display());
                let _ = writeln!(output, "total_lines: {total_lines}");
                let _ = writeln!(
                    output,
                    "total_bytes: {total_bytes} ({})",
                    format_size(total_bytes)
                );
                let _ = write!(output, "\n{streams_block}");
                let summary = format!(
                    "execution_id '{}' still running after {}s",
                    params.execution_id, timeout_secs
                );
                Ok(CallToolResult::success(vec![
                    Content::text(output).with_audience(vec![Role::Assistant]),
                    Content::text(summary).with_audience(vec![Role::User]),
                ]))
            }
        }
    }

    // -----------------------------------------------------------------------
    // terminate
    // -----------------------------------------------------------------------

    async fn terminate_impl(&self, params: TerminateParams) -> Result<CallToolResult, ErrorData> {
        let normalized = params.signal.as_deref().unwrap_or("SIGTERM").to_uppercase();

        #[cfg(unix)]
        {
            let (signum, signal_name) = match normalized.as_str() {
                "SIGTERM" | "TERM" => (libc::SIGTERM, "SIGTERM"),
                "SIGKILL" | "KILL" => (libc::SIGKILL, "SIGKILL"),
                "SIGINT" | "INT" => (libc::SIGINT, "SIGINT"),
                "SIGHUP" | "HUP" => (libc::SIGHUP, "SIGHUP"),
                other => {
                    return Err(ErrorData::invalid_params(
                        format!("unsupported signal: {other}"),
                        None,
                    ));
                }
            };

            let map = self.inner.spawned.lock().await;
            let entry = map.get(&params.execution_id).ok_or_else(|| {
                ErrorData::invalid_params(
                    format!(
                        "execution_id '{}' is not a tracked background process",
                        params.execution_id
                    ),
                    None,
                )
            })?;

            entry.child.signal(signum).map_err(|err| {
                internal_error(format!(
                    "failed to send {signal_name} to execution_id '{}': {err}",
                    params.execution_id
                ))
            })?;

            let mut output = String::new();
            let _ = writeln!(output, "execution_id: {}", params.execution_id);
            let _ = writeln!(output, "signal: {}", normalized);
            let _ = writeln!(output, "command: {}", entry.command);
            let _ = writeln!(
                output,
                "stdout_log_path: {}",
                entry.stdout_log_path.display()
            );
            let _ = write!(
                output,
                "stderr_log_path: {}",
                entry.stderr_log_path.display()
            );
            let summary = format!("sent {} to {}", normalized, params.execution_id);

            Ok(CallToolResult::success(vec![
                Content::text(output).with_audience(vec![Role::Assistant]),
                Content::text(summary).with_audience(vec![Role::User]),
            ]))
        }

        #[cfg(windows)]
        {
            let mut map = self.inner.spawned.lock().await;
            let entry = map.get_mut(&params.execution_id).ok_or_else(|| {
                ErrorData::invalid_params(
                    format!(
                        "execution_id '{}' is not a tracked background process",
                        params.execution_id
                    ),
                    None,
                )
            })?;

            if let Err(e) = entry.child.start_kill() {
                return Err(internal_error(format!(
                    "failed to terminate execution_id '{}': {e}",
                    params.execution_id
                )));
            }

            let command = entry.command.clone();
            let stdout_log_path = entry.stdout_log_path.clone();
            let stderr_log_path = entry.stderr_log_path.clone();

            let mut output = String::new();
            let _ = writeln!(output, "execution_id: {}", params.execution_id);
            let _ = writeln!(output, "signal: {}", normalized);
            let _ = writeln!(output, "command: {}", command);
            let _ = writeln!(output, "stdout_log_path: {}", stdout_log_path.display());
            let _ = write!(output, "stderr_log_path: {}", stderr_log_path.display());
            let summary = format!("sent {} to {}", normalized, params.execution_id);

            Ok(CallToolResult::success(vec![
                Content::text(output).with_audience(vec![Role::Assistant]),
                Content::text(summary).with_audience(vec![Role::User]),
            ]))
        }
    }

    // -----------------------------------------------------------------------
    // rollback_file
    // -----------------------------------------------------------------------

    async fn rollback_file_impl(
        &self,
        params: RollbackParams,
    ) -> Result<CallToolResult, ErrorData> {
        let roots = self.inner.roots.read().await;
        let path = validate_path(&params.repo_path, &roots).map_err(invalid_params)?;
        drop(roots);

        let commit_id = ObjectId::from_hex(params.commit_id.as_bytes())
            .map_err(|e| ErrorData::invalid_params(format!("invalid commit_id: {e}"), None))?;

        let repo_dir = harnx_mcp_history::discover::find_repo_for_path(&path).ok_or_else(|| {
            ErrorData::invalid_params("path is not inside a git repository".to_string(), None)
        })?;

        let new_commit_id = self
            .inner
            .history
            .rollback(&repo_dir, commit_id)
            .await
            .map_err(|e| ErrorData::internal_error(format!("rollback failed: {e}"), None))?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Rolled back to harnx snapshot {}; new commit {} created (can be reverted)",
            &params.commit_id[..8.min(params.commit_id.len())],
            new_commit_id.to_hex(),
        ))]))
    }

    // -----------------------------------------------------------------------
    // shared helpers
    // -----------------------------------------------------------------------

    async fn resolve_working_dir(&self, requested: Option<&str>) -> Result<PathBuf, ErrorData> {
        let roots = self.inner.roots.read().await;
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
                format!(
                    "cannot resolve working directory '{}': {err}",
                    resolved.display()
                ),
                None,
            )
        })?;

        if !canonical.is_dir() {
            return Err(ErrorData::invalid_params(
                format!(
                    "working directory '{}' is not a directory",
                    canonical.display()
                ),
                None,
            ));
        }

        if roots.is_empty()
            || !roots.iter().any(|root| {
                root.canonicalize()
                    .map(|canonical_root| canonical.starts_with(&canonical_root))
                    .unwrap_or(false)
            })
        {
            return Err(ErrorData::invalid_params(
                format!(
                    "working directory '{}' is outside allowed roots",
                    canonical.display()
                ),
                None,
            ));
        }

        Ok(canonical)
    }

    pub fn cleanup_log_dir(&self) -> std::io::Result<()> {
        if self.inner.log_dir.exists() {
            std::fs::remove_dir_all(&self.inner.log_dir)
        } else {
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// ServerHandler impl
// ---------------------------------------------------------------------------

impl ServerHandler for BashServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "harnx-mcp-bash",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Local shell command MCP server with output truncation and retrievable temp logs.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult {
            meta: None,
            tools: vec![
                Tool::new(
                    "exec",
                    "Execute a local bash command and return truncated combined stdout/stderr. When output is cropped, stdout/stderr temp log files are included for later retrieval.",
                    Map::new(),
                )
                .with_input_schema::<ExecCommandParams>()
                .with_meta(Meta(json!({
                    "call_template": "**$** `{{ args.command }}`{% if args.working_dir %} *(in {{ args.working_dir }})*{% endif %}",
                    "result_template": "{{ result.content[0].text | default('') }}"
                }).as_object().unwrap().clone())),
                Tool::new(
                    "read_exec_log",
                    "Read a temp stdout/stderr log file generated by this bash server. Use execution_id from exec/wait response with stream 'stdout' or 'stderr'. Supports offset/limit/tail/grep/head_lines/tail_lines/max_output_bytes, but only for server-owned temp logs.",
                    Map::new(),
                )
                .with_input_schema::<ReadExecLogParams>()
                .with_meta(Meta(json!({
                    "call_template": "**read log** `{{ args.execution_id }}/{{ args.stream }}`",
                    "result_template": "{{ result.content[0].text | default('') }}"
                }).as_object().unwrap().clone())),
                Tool::new(
                    "spawn",
                    "Spawn a background bash command. Returns an execution_id and log file paths immediately without waiting for the command to finish. Output is written to separate stdout.log and stderr.log files. Use 'wait' to check for completion and 'terminate' to stop it.",
                    Map::new(),
                )
                .with_input_schema::<SpawnCommandParams>()
                .with_meta(Meta(json!({
                    "call_template": "**spawn** `{{ args.command }}`{% if args.working_dir %} *(in {{ args.working_dir }})*{% endif %}",
                    "result_template": "{{ result.content[0].text | default('') }}"
                }).as_object().unwrap().clone())),
                Tool::new(
                    "wait",
                    "Wait for a spawned background process to exit. Returns the exit code, output metrics, and truncated output. If the process does not exit within the timeout, returns its current status and partial output without killing it.",
                    Map::new(),
                )
                .with_input_schema::<WaitParams>()
                .with_meta(Meta(json!({
                    "call_template": "**wait** for {{ args.execution_id }}",
                    "result_template": "{{ result.content[0].text | default('') }}"
                }).as_object().unwrap().clone())),
                Tool::new(
                    "terminate",
                    "Send a signal to a spawned background process. Default signal is SIGTERM. Supported signals: SIGTERM, SIGKILL, SIGINT, SIGHUP.",
                    Map::new(),
                )
                .with_input_schema::<TerminateParams>()
                .with_meta(Meta(json!({
                    "call_template": "**terminate** {{ args.execution_id }}{% if args.signal %} ({{ args.signal }}){% endif %}",
                    "result_template": "{{ result.content[0].text | default('') }}"
                }).as_object().unwrap().clone())),
                Tool::new(
                    "rollback_file",
                    "Restore a repository to a prior harnx history snapshot. Pass the commit SHA from the 'commit <sha>' line at the top of a prior tool response's diff as the commit_id parameter.",
                    Map::new(),
                )
                .with_input_schema::<RollbackParams>()
                .with_meta(Meta(json!({
                    "call_template": "**rollback_file** to `{{ args.commit_id | truncate(8, end='') }}`",
                    "result_template": "{{ result.content[0].text | default('') }}"
                }).as_object().unwrap().clone())),
            ],
            next_cursor: None,
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Err(err) = self.ensure_roots_initialized(&context.peer).await {
            eprintln!(
                "harnx-mcp-bash: failed to initialize roots: {}",
                err.message
            );
        }

        match request.name.as_ref() {
            "exec" => {
                let params = parse_arguments::<ExecCommandParams>(request.arguments)?;
                self.exec_command_impl(params).await
            }
            "read_exec_log" => {
                let params = parse_arguments::<ReadExecLogParams>(request.arguments)?;
                self.read_exec_log_impl(params).await
            }
            "spawn" => {
                let params = parse_arguments::<SpawnCommandParams>(request.arguments)?;
                self.spawn_impl(params).await
            }
            "wait" => {
                let params = parse_arguments::<WaitParams>(request.arguments)?;
                self.wait_impl(params).await
            }
            "terminate" => {
                let params = parse_arguments::<TerminateParams>(request.arguments)?;
                self.terminate_impl(params).await
            }
            "rollback_file" => {
                let params = parse_arguments::<RollbackParams>(request.arguments)?;
                self.rollback_file_impl(params).await
            }
            other => Err(ErrorData::invalid_params(
                format!("unknown tool: {other}"),
                None,
            )),
        }
    }

    fn on_roots_list_changed(
        &self,
        context: NotificationContext<RoleServer>,
    ) -> impl Future<Output = ()> + Send + '_ {
        let this = self.clone();
        async move {
            let peer = context.peer.clone();
            tokio::spawn(async move {
                if let Err(err) = this.refresh_roots(&peer).await {
                    eprintln!("harnx-mcp-bash: failed to refresh roots: {}", err.message);
                }
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

fn parse_arguments<T: DeserializeOwned>(
    arguments: Option<Map<String, Value>>,
) -> Result<T, ErrorData> {
    serde_json::from_value(Value::Object(arguments.unwrap_or_default()))
        .map_err(|err| ErrorData::invalid_params(format!("invalid tool arguments: {err}"), None))
}

fn tool_error(msg: impl Into<String>) -> Result<CallToolResult, ErrorData> {
    Ok(CallToolResult::error(vec![Content::text(msg.into())]))
}

fn invalid_params(msg: impl Into<Cow<'static, str>>) -> ErrorData {
    ErrorData::invalid_params(msg, None)
}

fn internal_error(msg: impl Into<Cow<'static, str>>) -> ErrorData {
    ErrorData::internal_error(msg, None)
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

async fn read_pipe_to_file<R>(mut reader: R, mut writer: TokioFile) -> std::io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 8192];

    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }

        writer.write_all(&buffer[..read]).await?;
        bytes.extend_from_slice(&buffer[..read]);
    }

    writer.flush().await?;
    writer.sync_all().await?;
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

fn count_lines(s: &str) -> usize {
    if s.is_empty() {
        0
    } else {
        s.lines().count()
    }
}

/// Render one stream's output block with `===== stdout =====` / `===== /stdout =====` markers.
/// Each stream is truncated independently using `truncate_opts`.
/// Returns the rendered block string (no trailing newline).
fn render_stream_block(
    name: &str,
    content: &str,
    truncate_opts: &TruncateOpts,
    log_hint: Option<(&str, &Path)>, // (execution_id, log_path) for truncation hint
) -> String {
    let sanitized = sanitize_output_text(content);
    if sanitized.is_empty() {
        return format!("===== {name} (empty) =====");
    }
    let truncated = truncate_output(&sanitized, truncate_opts);
    let was_truncated = truncated != sanitized;
    let mut block = format!("===== {name} =====\n{truncated}");
    if was_truncated {
        if let Some((execution_id, log_path)) = log_hint {
            let _ = write!(
                block,
                "\n\n[{name} truncated from {} to {}. Use max_output_bytes, head_lines, or tail_lines to see more; full log via read_exec_log: execution_id={execution_id}, stream={name} ({})]",
                format_size(sanitized.len()),
                format_size(truncated.len()),
                log_path.display()
            );
        } else {
            let _ = write!(
                block,
                "\n\n[{name} truncated from {} to {}. Use max_output_bytes, head_lines, or tail_lines to see more]",
                format_size(sanitized.len()),
                format_size(truncated.len())
            );
        }
    }
    let _ = write!(block, "\n===== /{name} =====");
    block
}

/// Render separate stdout and stderr blocks, each truncated independently.
/// Returns (rendered_string, stdout_lines, stderr_lines, stdout_bytes, stderr_bytes).
/// Apply a grep regex filter to each line of `content`, returning only matching lines joined by `\n`.
/// Lines that fail regex evaluation are kept (fail-open).
fn grep_filter(content: &str, grep_regex: &Regex) -> String {
    content
        .lines()
        .filter(|line| grep_regex.is_match(line).unwrap_or(true))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Render separate stdout and stderr blocks, each grep-filtered and truncated independently.
/// Returns (rendered_string, stdout_lines, stderr_lines, stdout_bytes, stderr_bytes).
/// Metrics reflect post-grep content so callers see accurate totals.
fn render_streams_block(
    stdout: &str,
    stderr: &str,
    truncate_opts: &TruncateOpts,
    grep_regex: Option<&Regex>,
    execution_id: &str,
    stdout_log_path: &Path,
    stderr_log_path: &Path,
) -> (String, usize, usize, usize, usize) {
    let stdout_filtered = grep_regex
        .map(|r| grep_filter(stdout, r))
        .unwrap_or_else(|| stdout.to_owned());
    let stderr_filtered = grep_regex
        .map(|r| grep_filter(stderr, r))
        .unwrap_or_else(|| stderr.to_owned());

    let stdout_lines = count_lines(&stdout_filtered);
    let stderr_lines = count_lines(&stderr_filtered);
    let stdout_bytes = stdout_filtered.len();
    let stderr_bytes = stderr_filtered.len();

    let stdout_block = render_stream_block(
        "stdout",
        &stdout_filtered,
        truncate_opts,
        Some((execution_id, stdout_log_path)),
    );
    let stderr_block = render_stream_block(
        "stderr",
        &stderr_filtered,
        truncate_opts,
        Some((execution_id, stderr_log_path)),
    );

    let rendered = format!("{stdout_block}\n{stderr_block}");
    (
        rendered,
        stdout_lines,
        stderr_lines,
        stdout_bytes,
        stderr_bytes,
    )
}

struct TimeoutRenderContext<'a> {
    working_dir: &'a Path,
    execution_id: &'a str,
    timeout_secs: u64,
    total_lines: usize,
    total_bytes: usize,
    stdout: &'a str,
    stderr: &'a str,
    truncate_opts: &'a TruncateOpts,
    stdout_log_path: &'a Path,
    stderr_log_path: &'a Path,
}

fn render_timeout_message(ctx: TimeoutRenderContext<'_>) -> String {
    let TimeoutRenderContext {
        working_dir,
        execution_id,
        timeout_secs,
        total_lines,
        total_bytes,
        stdout,
        stderr,
        truncate_opts,
        stdout_log_path,
        stderr_log_path,
    } = ctx;
    let mut output = String::new();
    let _ = writeln!(
        output,
        "command timed out after {timeout_secs}s and was terminated"
    );
    let _ = writeln!(output, "execution_id: {execution_id}");
    let _ = writeln!(output, "working_dir: {}", working_dir.display());
    let _ = writeln!(output, "stdout_log_path: {}", stdout_log_path.display());
    let _ = writeln!(output, "stderr_log_path: {}", stderr_log_path.display());
    let _ = writeln!(output, "total_lines: {total_lines}");
    let _ = writeln!(
        output,
        "total_bytes: {total_bytes} ({})",
        format_size(total_bytes)
    );
    let (streams_block, _, _, _, _) = render_streams_block(
        stdout,
        stderr,
        truncate_opts,
        None,
        execution_id,
        stdout_log_path,
        stderr_log_path,
    );
    let _ = write!(output, "\n{streams_block}");
    output
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(all(test, not(target_os = "windows")))]
mod tests {
    use super::*;

    use rmcp::handler::client::ClientHandler;
    use rmcp::model::{ClientCapabilities, InitializeRequestParams, ListRootsResult, Root};
    use rmcp::service::{
        serve_client, serve_server, RequestContext, RoleClient, RoleServer, RunningService,
    };
    use tokio::io::duplex;
    use uuid::Uuid;

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("harnx-mcp-bash-test-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[derive(Clone, Default)]
    #[allow(dead_code)]
    struct TestClientHandler {
        roots: Vec<PathBuf>,
    }

    impl TestClientHandler {
        #[allow(dead_code)]
        fn new(roots: Vec<PathBuf>) -> Self {
            Self { roots }
        }
    }

    impl ClientHandler for TestClientHandler {
        fn get_info(&self) -> InitializeRequestParams {
            InitializeRequestParams::new(
                ClientCapabilities::builder()
                    .enable_roots()
                    .enable_roots_list_changed()
                    .build(),
                Implementation::new("test", "0.1"),
            )
        }

        async fn list_roots(
            &self,
            _cx: RequestContext<RoleClient>,
        ) -> Result<ListRootsResult, ErrorData> {
            Ok(ListRootsResult::new(
                self.roots
                    .iter()
                    .map(|root| {
                        Root::new(format!("file://{}", root.canonicalize().unwrap().display()))
                    })
                    .collect(),
            ))
        }
    }

    #[allow(dead_code)]
    struct TestConnection {
        _server_service: RunningService<RoleServer, BashServer>,
        client_service: RunningService<RoleClient, TestClientHandler>,
    }

    #[allow(dead_code)]
    async fn connect_server(server: BashServer, roots: Vec<PathBuf>) -> TestConnection {
        let (client_transport, server_transport) = duplex(65_536);
        let server_fut = serve_server(server, server_transport);
        let client_fut = serve_client(TestClientHandler::new(roots), client_transport);
        let (server_res, client_res) = tokio::join!(server_fut, client_fut);

        TestConnection {
            _server_service: server_res.unwrap(),
            client_service: client_res.unwrap(),
        }
    }

    fn text_content(result: &CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|content| match &content.raw {
                rmcp::model::RawContent::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn test_bash_tools_have_meta_templates() {
        let tools = vec![
            Tool::new(
                "exec",
                "Execute a local bash command and return truncated combined stdout/stderr. When output is cropped, stdout/stderr temp log files are included for later retrieval.",
                Map::new(),
            )
            .with_input_schema::<ExecCommandParams>()
            .with_meta(Meta(json!({
                "call_template": "**$** `{{ args.command }}`{% if args.working_dir %} *(in {{ args.working_dir }})*{% endif %}",
                "result_template": "{{ result.content[0].text | default('') }}"
            }).as_object().unwrap().clone())),
            Tool::new(
                "read_exec_log",
                "Read a temp stdout/stderr log file generated by this bash server. Supports offset/limit/tail/grep/head_lines/tail_lines/max_output_bytes, but only for server-owned temp logs.",
                Map::new(),
            )
            .with_input_schema::<ReadExecLogParams>()
            .with_meta(Meta(json!({
                "call_template": "**read log** `{{ args.path }}`",
                "result_template": "{{ result.content[0].text | default('') }}"
            }).as_object().unwrap().clone())),
            Tool::new(
                "spawn",
                "Spawn a background bash command. Returns the PID and log file path immediately without waiting for the command to finish. Output is written to a log file. Use 'wait' to check for completion and 'terminate' to stop it.",
                Map::new(),
            )
            .with_input_schema::<SpawnCommandParams>()
            .with_meta(Meta(json!({
                "call_template": "**spawn** `{{ args.command }}`{% if args.working_dir %} *(in {{ args.working_dir }})*{% endif %}",
                "result_template": "{{ result.content[0].text | default('') }}"
            }).as_object().unwrap().clone())),
            Tool::new(
                "wait",
                "Wait for a spawned background process to exit. Returns the exit code and tail of the log file. If the process does not exit within the timeout, returns its current status and log tail without killing it.",
                Map::new(),
            )
            .with_input_schema::<WaitParams>()
            .with_meta(Meta(json!({
                "call_template": "**wait** for {{ args.execution_id }}",
                "result_template": "{{ result.content[0].text | default('') }}"
            }).as_object().unwrap().clone())),
            Tool::new(
                "terminate",
                "Send a signal to a spawned background process. Default signal is SIGTERM. Supported signals: SIGTERM, SIGKILL, SIGINT, SIGHUP.",
                Map::new(),
            )
            .with_input_schema::<TerminateParams>()
            .with_meta(Meta(json!({
                "call_template": "**terminate** {{ args.execution_id }}{% if args.signal %} ({{ args.signal }}){% endif %}",
                "result_template": "{{ result.content[0].text | default('') }}"
            }).as_object().unwrap().clone())),
        ];

        for tool in &tools {
            let meta = tool
                .meta
                .as_ref()
                .unwrap_or_else(|| panic!("tool '{}' has no _meta", tool.name));
            assert!(
                meta.0.contains_key("call_template"),
                "tool '{}' missing call_template in _meta",
                tool.name
            );
            assert!(
                meta.0.contains_key("result_template"),
                "tool '{}' missing result_template in _meta",
                tool.name
            );
        }
    }

    fn extract_field(text: &str, field: &str) -> String {
        text.lines()
            .find_map(|line| line.strip_prefix(&format!("{field}: ")))
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn test_working_dir_rejected_outside_roots() {
        let allowed = TestDir::new();
        let outside = TestDir::new();
        let server = BashServer::new(vec![allowed.path().to_path_buf()]);

        let result = server
            .exec_command_impl(ExecCommandParams {
                command: "pwd".to_string(),
                working_dir: Some(outside.path().to_string_lossy().to_string()),
                timeout_secs: Some(5),
                head_lines: None,
                tail_lines: None,
                max_output_bytes: None,
            })
            .await;

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .message
            .contains("outside allowed roots"));
    }

    #[tokio::test]
    async fn test_exec_rejects_empty_command() {
        let temp_dir = TestDir::new();
        let server = BashServer::new(vec![temp_dir.path().to_path_buf()]);

        let result = server
            .exec_command_impl(ExecCommandParams {
                command: "   ".to_string(),
                working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
                timeout_secs: Some(5),
                head_lines: None,
                tail_lines: None,
                max_output_bytes: None,
            })
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_exec_nonzero_exit_code() {
        let temp_dir = TestDir::new();
        let server = BashServer::new(vec![temp_dir.path().to_path_buf()]);

        let result = server
            .exec_command_impl(ExecCommandParams {
                command: "echo boom >&2; exit 1".to_string(),
                working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
                timeout_secs: Some(5),
                head_lines: None,
                tail_lines: None,
                max_output_bytes: None,
            })
            .await
            .unwrap();

        let text = text_content(&result);
        assert_eq!(result.is_error, Some(false));
        assert!(text.contains("exit_code: 1"));
    }

    #[tokio::test]
    async fn test_exec_basic_command() {
        let temp_dir = TestDir::new();
        let server = BashServer::new(vec![temp_dir.path().to_path_buf()]);

        let result = server
            .exec_command_impl(ExecCommandParams {
                command: "echo test".to_string(),
                working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
                timeout_secs: Some(5),
                head_lines: None,
                tail_lines: None,
                max_output_bytes: None,
            })
            .await
            .unwrap();

        let text = text_content(&result);
        assert_eq!(result.is_error, Some(false));
        assert!(text.contains("exit_code: 0"));
        assert!(text.contains("test"));

        let stdout_log_path = PathBuf::from(extract_field(&text, "stdout_log_path"));
        let stderr_log_path = PathBuf::from(extract_field(&text, "stderr_log_path"));
        assert!(stdout_log_path.exists());
        assert!(stderr_log_path.exists());
    }

    #[tokio::test]
    async fn test_exec_timeout() {
        let temp_dir = TestDir::new();
        let server = BashServer::new(vec![temp_dir.path().to_path_buf()]);

        let result = server
            .exec_command_impl(ExecCommandParams {
                command: "sleep 10".to_string(),
                working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
                timeout_secs: Some(1),
                head_lines: None,
                tail_lines: None,
                max_output_bytes: None,
            })
            .await
            .unwrap();

        let text = text_content(&result);
        assert_eq!(result.is_error, Some(true));
        assert!(text.contains("command timed out after 1s and was terminated"));
        assert!(text.contains("stdout_log_path:"));
        assert!(text.contains("stderr_log_path:"));
    }

    #[tokio::test]
    async fn test_exec_truncation_mentions_log_paths_and_read_exec_log_works() {
        let temp_dir = TestDir::new();
        let server = BashServer::new(vec![temp_dir.path().to_path_buf()]);

        let result = server
            .exec_command_impl(ExecCommandParams {
                command: "printf 'out1\nout2\nout3\n'; printf 'err1\nerr2\n' >&2".to_string(),
                working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
                timeout_secs: Some(5),
                head_lines: Some(1),
                tail_lines: Some(1),
                max_output_bytes: Some(16),
            })
            .await
            .unwrap();

        let text = text_content(&result);
        let execution_id = extract_field(&text, "execution_id");
        assert!(text.contains("full log via read_exec_log"));
        assert!(text.contains(&execution_id));

        let stdout_read = server
            .read_exec_log_impl(ReadExecLogParams {
                execution_id: execution_id.clone(),
                stream: "stdout".to_string(),
                offset: None,
                limit: None,
                tail: None,
                grep: None,
                head_lines: None,
                tail_lines: None,
                max_output_bytes: None,
            })
            .await
            .unwrap();
        let stdout_text = text_content(&stdout_read);
        assert!(stdout_text.contains("1: out1"));
        assert!(stdout_text.contains("2: out2"));
        assert!(stdout_text.contains("3: out3"));

        let stderr_read = server
            .read_exec_log_impl(ReadExecLogParams {
                execution_id,
                stream: "stderr".to_string(),
                offset: None,
                limit: None,
                tail: Some(1),
                grep: None,
                head_lines: None,
                tail_lines: None,
                max_output_bytes: None,
            })
            .await
            .unwrap();
        let stderr_text = text_content(&stderr_read);
        assert!(stderr_text.contains("2: err2"));
        assert!(stderr_text.contains("showing last 1 of 2 matching lines"));
    }

    #[tokio::test]
    async fn test_read_exec_log_rejects_invalid_stream() {
        let temp_dir = TestDir::new();
        let server = BashServer::new(vec![temp_dir.path().to_path_buf()]);

        let result = server
            .read_exec_log_impl(ReadExecLogParams {
                execution_id: "exec-test".to_string(),
                stream: "invalid".to_string(),
                offset: None,
                limit: None,
                tail: None,
                grep: None,
                head_lines: None,
                tail_lines: None,
                max_output_bytes: None,
            })
            .await;

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .message
            .contains("stream must be 'stdout' or 'stderr'"));
    }

    #[tokio::test]
    async fn test_cleanup_log_dir_removes_temp_logs() {
        let temp_dir = TestDir::new();
        let server = BashServer::new(vec![temp_dir.path().to_path_buf()]);

        let result = server
            .exec_command_impl(ExecCommandParams {
                command: "echo cleanup".to_string(),
                working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
                timeout_secs: Some(5),
                head_lines: None,
                tail_lines: None,
                max_output_bytes: None,
            })
            .await
            .unwrap();

        let text = text_content(&result);
        let stdout_log_path = PathBuf::from(extract_field(&text, "stdout_log_path"));
        let log_dir = stdout_log_path.parent().unwrap().to_path_buf();
        assert!(log_dir.exists());

        server.cleanup_log_dir().unwrap();
        assert!(!log_dir.exists());
    }

    #[tokio::test]
    async fn test_spawn_and_wait() {
        let temp_dir = TestDir::new();
        let server = BashServer::new(vec![temp_dir.path().to_path_buf()]);

        let result = server
            .spawn_impl(SpawnCommandParams {
                command: "echo background && sleep 1".to_string(),
                working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
            })
            .await
            .unwrap();

        let text = text_content(&result);
        let execution_id = extract_field(&text, "execution_id");

        let result = server
            .wait_impl(WaitParams {
                execution_id,
                timeout_secs: Some(5),
                head_lines: None,
                tail_lines: Some(10),
                max_output_bytes: None,
                grep: None,
            })
            .await
            .unwrap();

        let text = text_content(&result);
        assert!(text.contains("status: exited"));
        assert!(text.contains("background"));
    }

    #[tokio::test]
    async fn test_spawn_wait_timeout() {
        let temp_dir = TestDir::new();
        let server = BashServer::new(vec![temp_dir.path().to_path_buf()]);

        let result = server
            .spawn_impl(SpawnCommandParams {
                command: "sleep 5".to_string(),
                working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
            })
            .await
            .unwrap();

        let text = text_content(&result);
        let execution_id = extract_field(&text, "execution_id");

        let result = server
            .wait_impl(WaitParams {
                execution_id,
                timeout_secs: Some(1),
                head_lines: None,
                tail_lines: Some(10),
                max_output_bytes: None,
                grep: None,
            })
            .await
            .unwrap();

        let text = text_content(&result);
        assert!(text.contains("status: running"));
    }

    #[tokio::test]
    async fn test_spawn_and_terminate() {
        let temp_dir = TestDir::new();
        let server = BashServer::new(vec![temp_dir.path().to_path_buf()]);

        let result = server
            .spawn_impl(SpawnCommandParams {
                command: "sleep 30".to_string(),
                working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
            })
            .await
            .unwrap();

        let text = text_content(&result);
        let execution_id = extract_field(&text, "execution_id");

        let result = server
            .terminate_impl(TerminateParams {
                execution_id,
                signal: Some("SIGTERM".to_string()),
            })
            .await
            .unwrap();

        let text = text_content(&result);
        assert!(text.contains("signal: SIGTERM"));
    }

    #[tokio::test]
    async fn test_wait_unknown_execution_id() {
        let temp_dir = TestDir::new();
        let server = BashServer::new(vec![temp_dir.path().to_path_buf()]);

        let result = server
            .wait_impl(WaitParams {
                execution_id: "exec-does-not-exist".to_string(),
                timeout_secs: Some(1),
                head_lines: None,
                tail_lines: None,
                max_output_bytes: None,
                grep: None,
            })
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_terminate_unknown_execution_id() {
        let temp_dir = TestDir::new();
        let server = BashServer::new(vec![temp_dir.path().to_path_buf()]);

        let result = server
            .terminate_impl(TerminateParams {
                execution_id: "exec-does-not-exist".to_string(),
                signal: None,
            })
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_spawn_with_output() {
        let temp_dir = TestDir::new();
        let server = BashServer::new(vec![temp_dir.path().to_path_buf()]);

        let result = server
            .spawn_impl(SpawnCommandParams {
                command: "for i in 1 2 3; do echo line$i; done".to_string(),
                working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
            })
            .await
            .unwrap();

        let text = text_content(&result);
        let execution_id = extract_field(&text, "execution_id");

        let result = server
            .wait_impl(WaitParams {
                execution_id,
                timeout_secs: Some(5),
                head_lines: None,
                tail_lines: Some(10),
                max_output_bytes: None,
                grep: None,
            })
            .await
            .unwrap();

        let text = text_content(&result);
        assert!(text.contains("exit_code: 0"));
        assert!(text.contains("line1"));
        assert!(text.contains("line2"));
        assert!(text.contains("line3"));
    }
}

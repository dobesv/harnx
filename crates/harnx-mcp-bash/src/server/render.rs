// Auto-split from server.rs for cohesion. See server/mod.rs.
use super::*;

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

/// Parse a shebang line from the first line of `command`.
///
/// Returns `None` if the command does not start with `#!`.
/// Returns `Some((interpreter_path, extra_args))` where:
/// - `interpreter_path` is a bare name (for `#!/usr/bin/env INTERP`) or absolute path
/// - `extra_args` are any additional arguments on the shebang line (e.g. `-u`)
pub(crate) fn parse_shebang(command: &str) -> Option<(PathBuf, Vec<String>)> {
    let first_line = command.lines().next()?;
    let shebang_rest = first_line.strip_prefix("#!")?;

    let mut parts = shebang_rest.split_whitespace();
    let interpreter = parts.next()?;

    if interpreter == "/usr/bin/env" {
        // `#!/usr/bin/env [-flags...] INTERP [args...]` — skip any env flags (e.g. -S)
        // and use the first non-flag token as the interpreter name.
        let env_interp = parts.find(|t| !t.starts_with('-'))?;
        // Remaining tokens after the interpreter are extra args.
        let extra_args: Vec<String> = parts.map(str::to_string).collect();
        Some((PathBuf::from(env_interp), extra_args))
    } else {
        // `#!/path/to/INTERP [args...]` — use literal path
        Some((
            PathBuf::from(interpreter),
            parts.map(str::to_string).collect(),
        ))
    }
}

/// Return the file extension (without dot) for the temp script file based on the shebang interpreter.
pub(crate) fn shebang_script_ext(command: &str) -> &'static str {
    let Some((interp, _)) = parse_shebang(command) else {
        return "sh";
    };
    match interp
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
    {
        "python" | "python3" => "py",
        "node" | "nodejs" | "bun" => "js",
        "ruby" => "rb",
        "perl" => "pl",
        "deno" => "ts",
        "php" => "php",
        _ => "sh",
    }
}

pub(crate) fn parse_arguments<T: DeserializeOwned>(
    arguments: Option<Map<String, Value>>,
) -> Result<T, ErrorData> {
    serde_json::from_value(Value::Object(arguments.unwrap_or_default()))
        .map_err(|err| ErrorData::invalid_params(format!("invalid tool arguments: {err}"), None))
}

pub(crate) fn tool_error(msg: impl Into<String>) -> Result<CallToolResult, ErrorData> {
    Ok(CallToolResult::error(vec![Content::text(msg.into())]))
}

pub(crate) fn invalid_params(msg: impl Into<Cow<'static, str>>) -> ErrorData {
    ErrorData::invalid_params(msg, None)
}

pub(crate) fn internal_error(msg: impl Into<Cow<'static, str>>) -> ErrorData {
    ErrorData::internal_error(msg, None)
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) fn sandbox_run_test_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/harnx-sandbox-exec")
}

pub(crate) fn load_bash_env_file() -> Vec<(String, String)> {
    let env_file = harnx_core::config_paths::bash_env_file();
    let Ok(contents) = std::fs::read_to_string(&env_file) else {
        return vec![];
    };
    #[cfg(unix)]
    {
        use std::os::unix::prelude::PermissionsExt;
        let _ = std::fs::set_permissions(&env_file, std::fs::Permissions::from_mode(0o600));
    }
    contents
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                return None;
            }

            let mut parts = trimmed.splitn(2, '=');
            let key = parts.next()?.trim();
            let value = parts.next()?.trim();
            if key.is_empty() {
                return None;
            }
            Some((key.to_string(), value.to_string()))
        })
        .collect()
}

pub(crate) async fn read_pipe_to_file<R>(
    mut reader: R,
    mut writer: TokioFile,
) -> std::io::Result<Vec<u8>>
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

pub(crate) async fn join_pipe(
    task: tokio::task::JoinHandle<std::io::Result<Vec<u8>>>,
    name: &str,
) -> Result<Vec<u8>, ErrorData> {
    task.await
        .map_err(|err| internal_error(format!("failed to join {name} reader task: {err}")))?
        .map_err(|err| internal_error(format!("failed to read {name}: {err}")))
}

pub(crate) fn count_lines(s: &str) -> usize {
    if s.is_empty() {
        0
    } else {
        s.lines().count()
    }
}

/// Render one stream's output block inside markdown fences with HTML markers.
/// Each stream is truncated independently using `truncate_opts`.
/// Returns the rendered block string (no trailing newline).
pub(crate) fn render_stream_block(
    name: &str,
    content: &str,
    truncate_opts: &TruncateOpts,
    log_hint: Option<(&str, &Path)>, // (execution_id, log_path) for truncation hint
) -> String {
    let sanitized = sanitize_output_text(content);
    let truncated = truncate_output(&sanitized, truncate_opts);
    let was_truncated = truncated != sanitized;
    let mut block = format!("<!-- start {name} -->\n```\n{truncated}");
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
    // Known limitation: embedded ``` in stream content can break fence parsing;
    // HTML comment markers provide structural fallback for downstream consumers.
    let _ = write!(block, "\n```\n<!-- end {name} -->");
    block
}

/// Render separate stdout and stderr blocks, each truncated independently.
/// Returns (rendered_string, stdout_lines, stderr_lines, stdout_bytes, stderr_bytes).
/// Apply a grep regex filter to each line of `content`, returning only matching lines joined by `\n`.
/// Lines that fail regex evaluation are kept (fail-open).
pub(crate) fn grep_filter(content: &str, grep_regex: &Regex) -> String {
    content
        .lines()
        .filter(|line| grep_regex.is_match(line).unwrap_or(true))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Render separate stdout and stderr blocks, each grep-filtered and truncated independently.
/// Returns (rendered_string, stdout_lines, stderr_lines, stdout_bytes, stderr_bytes).
/// Metrics reflect post-grep content so callers see accurate totals.
pub(crate) fn render_streams_block(
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

    let rendered = format!("{stdout_block}\n\n{stderr_block}");
    (
        rendered,
        stdout_lines,
        stderr_lines,
        stdout_bytes,
        stderr_bytes,
    )
}

#[derive(serde::Serialize)]
pub(crate) struct MetadataHeader<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) execution_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) status: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) command: Option<&'a str>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_optional_path"
    )]
    pub(crate) working_dir: Option<&'a Path>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_optional_path"
    )]
    pub(crate) stdout_log_path: Option<&'a Path>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_optional_path"
    )]
    pub(crate) stderr_log_path: Option<&'a Path>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) total_lines: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) total_bytes: Option<usize>,
}

fn serialize_optional_path<S>(path: &Option<&Path>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match path {
        Some(path) => serializer.serialize_str(&path.display().to_string()),
        None => serializer.serialize_none(),
    }
}

pub(crate) fn render_metadata_header(output: &mut String, metadata: MetadataHeader<'_>) {
    let yaml = serde_yaml::to_string(&metadata).expect("MetadataHeader should serialize to YAML");
    let _ = write!(output, "```yaml\n{yaml}```");
}

pub(crate) struct TimeoutRenderContext<'a> {
    pub(crate) command: &'a str,
    pub(crate) working_dir: &'a Path,
    pub(crate) execution_id: &'a str,
    pub(crate) timeout_secs: u64,
    pub(crate) total_lines: usize,
    pub(crate) total_bytes: usize,
    pub(crate) stdout: &'a str,
    pub(crate) stderr: &'a str,
    pub(crate) truncate_opts: &'a TruncateOpts,
    pub(crate) stdout_log_path: &'a Path,
    pub(crate) stderr_log_path: &'a Path,
}

pub(crate) fn render_timeout_message(ctx: TimeoutRenderContext<'_>) -> String {
    let TimeoutRenderContext {
        command,
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
    render_metadata_header(
        &mut output,
        MetadataHeader {
            execution_id: Some(execution_id),
            status: Some("timeout"),
            exit_code: None,
            command: Some(command),
            working_dir: Some(working_dir),
            stdout_log_path: Some(stdout_log_path),
            stderr_log_path: Some(stderr_log_path),
            total_lines: Some(total_lines),
            total_bytes: Some(total_bytes),
        },
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

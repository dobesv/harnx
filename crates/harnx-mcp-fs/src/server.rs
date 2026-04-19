use harnx_mcp::safety::{
    file_uri_to_path, format_size, is_binary_content, sanitize_output_text, truncate_line,
    truncate_output, validate_path, validate_write_path, TruncateOpts, DEFAULT_FIND_LIMIT,
    DEFAULT_GREP_LIMIT, DEFAULT_LS_LIMIT, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES,
    GREP_MAX_LINE_LENGTH, LS_SCAN_HARD_LIMIT, READ_MAX_FILE_BYTES, SEARCH_FILE_MAX_BYTES,
    WRITE_MAX_BYTES,
};

use fancy_regex::Regex;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, ErrorData, Implementation, ListToolsResult,
    PaginatedRequestParams, Role, ServerCapabilities, ServerInfo, Tool, ToolAnnotations,
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Deserialize)]
pub struct ReadFileParams {
    pub path: String,
    #[serde(default)]
    pub offset: Option<usize>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub tail: Option<usize>,
    #[serde(default)]
    pub grep: Option<String>,
    #[serde(default)]
    pub head_lines: Option<usize>,
    #[serde(default)]
    pub tail_lines: Option<usize>,
    #[serde(default)]
    pub max_output_bytes: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct WriteFileParams {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Deserialize)]
pub struct EditFileParams {
    pub path: String,
    pub old_text: String,
    pub new_text: String,
    #[serde(default)]
    pub replace_all: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct ListDirectoryParams {
    pub path: String,
    #[serde(default)]
    pub recursive: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct SearchFilesParams {
    pub pattern: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub include: Option<String>,
    #[serde(default)]
    pub context_lines: Option<usize>,
    #[serde(default)]
    pub ignore_case: Option<bool>,
    #[serde(default)]
    pub max_results: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct FindFilesParams {
    pub pattern: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub max_results: Option<usize>,
}

impl JsonSchema for ReadFileParams {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("ReadFileParams")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let path = generator.subschema_for::<String>();
        let offset = generator.subschema_for::<Option<usize>>();
        let limit = generator.subschema_for::<Option<usize>>();
        let tail = generator.subschema_for::<Option<usize>>();
        let grep = generator.subschema_for::<Option<String>>();
        let head_lines = generator.subschema_for::<Option<usize>>();
        let tail_lines = generator.subschema_for::<Option<usize>>();
        let max_output_bytes = generator.subschema_for::<Option<usize>>();
        object_schema(
            vec![
                ("path", path),
                ("offset", offset),
                ("limit", limit),
                ("tail", tail),
                ("grep", grep),
                ("head_lines", head_lines),
                ("tail_lines", tail_lines),
                ("max_output_bytes", max_output_bytes),
            ],
            &["path"],
        )
    }
}

impl JsonSchema for WriteFileParams {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("WriteFileParams")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let path = generator.subschema_for::<String>();
        let content = generator.subschema_for::<String>();
        object_schema(
            vec![("path", path), ("content", content)],
            &["path", "content"],
        )
    }
}

impl JsonSchema for EditFileParams {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("EditFileParams")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let path = generator.subschema_for::<String>();
        let old_text = generator.subschema_for::<String>();
        let new_text = generator.subschema_for::<String>();
        let replace_all = generator.subschema_for::<Option<bool>>();
        object_schema(
            vec![
                ("path", path),
                ("old_text", old_text),
                ("new_text", new_text),
                ("replace_all", replace_all),
            ],
            &["path", "old_text", "new_text"],
        )
    }
}

impl JsonSchema for ListDirectoryParams {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("ListDirectoryParams")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let path = generator.subschema_for::<String>();
        let recursive = generator.subschema_for::<Option<bool>>();
        object_schema(vec![("path", path), ("recursive", recursive)], &["path"])
    }
}

impl JsonSchema for SearchFilesParams {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("SearchFilesParams")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let pattern = generator.subschema_for::<String>();
        let path = generator.subschema_for::<Option<String>>();
        let include = generator.subschema_for::<Option<String>>();
        let context_lines = generator.subschema_for::<Option<usize>>();
        let ignore_case = generator.subschema_for::<Option<bool>>();
        let max_results = generator.subschema_for::<Option<usize>>();
        object_schema(
            vec![
                ("pattern", pattern),
                ("path", path),
                ("include", include),
                ("context_lines", context_lines),
                ("ignore_case", ignore_case),
                ("max_results", max_results),
            ],
            &["pattern"],
        )
    }
}

impl JsonSchema for FindFilesParams {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("FindFilesParams")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let pattern = generator.subschema_for::<String>();
        let path = generator.subschema_for::<Option<String>>();
        let max_results = generator.subschema_for::<Option<usize>>();
        object_schema(
            vec![
                ("pattern", pattern),
                ("path", path),
                ("max_results", max_results),
            ],
            &["pattern"],
        )
    }
}

#[derive(Clone)]
pub struct FsServer {
    roots: Arc<RwLock<Vec<PathBuf>>>,
    roots_initialized: Arc<AtomicBool>,
}

impl FsServer {
    pub fn new(initial_roots: Vec<PathBuf>) -> Self {
        Self {
            roots: Arc::new(RwLock::new(initial_roots)),
            roots_initialized: Arc::new(AtomicBool::new(false)),
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

    async fn read_file_impl(&self, params: ReadFileParams) -> Result<CallToolResult, ErrorData> {
        if params.offset.is_some() && params.tail.is_some() {
            return Err(ErrorData::invalid_params(
                "offset and tail are mutually exclusive",
                None,
            ));
        }

        let roots = self.roots.read().await;
        let path = validate_path(&params.path, &roots).map_err(invalid_params)?;
        drop(roots);

        let metadata = std::fs::metadata(&path)
            .map_err(|err| internal_error(format!("cannot access '{}': {err}", params.path)))?;

        if !metadata.is_file() {
            return tool_error(format!(
                "'{}' is not a regular file. Use list_directory for directories.",
                params.path
            ));
        }

        if metadata.len() > READ_MAX_FILE_BYTES {
            return tool_error(format!(
                "File too large ({} bytes, max {}). Use offset/limit, tail, or search_files.",
                metadata.len(),
                format_size(READ_MAX_FILE_BYTES as usize)
            ));
        }

        let bytes = std::fs::read(&path)
            .map_err(|err| internal_error(format!("failed to read '{}': {err}", params.path)))?;

        if is_binary_content(&bytes) {
            return tool_error(format!("'{}' appears to be a binary file.", params.path));
        }

        let text = sanitize_output_text(&String::from_utf8_lossy(&bytes));
        let all_lines = text.lines().collect::<Vec<_>>();

        if all_lines.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "{} (empty file, 0 lines)",
                params.path
            ))]));
        }

        let grep_regex = if let Some(pattern) = params.grep.as_deref() {
            Some(Regex::new(pattern).map_err(|err| {
                ErrorData::invalid_params(format!("invalid grep regex '{pattern}': {err}"), None)
            })?)
        } else {
            None
        };

        let mut numbered_lines = all_lines
            .iter()
            .enumerate()
            .filter_map(|(index, line)| match &grep_regex {
                Some(regex) => match regex.is_match(line) {
                    Ok(true) => Some((index + 1, *line)),
                    Ok(false) => None,
                    Err(_) => None,
                },
                None => Some((index + 1, *line)),
            })
            .collect::<Vec<_>>();

        if numbered_lines.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No matching lines found".to_string(),
            )]));
        }

        let total_matching_lines = numbered_lines.len();
        let mut notices = Vec::new();

        if let Some(tail_count) = params.tail {
            if tail_count < total_matching_lines {
                notices.push(format!(
                    "showing last {} of {} matching lines",
                    tail_count, total_matching_lines
                ));
            }
            let start = total_matching_lines.saturating_sub(tail_count);
            numbered_lines = numbered_lines.into_iter().skip(start).collect();
        } else {
            let offset = params.offset.unwrap_or(1).max(1);
            let limit = params.limit.unwrap_or(DEFAULT_MAX_LINES);

            if offset > total_matching_lines {
                return tool_error(format!(
                    "Offset {} is beyond end of result set ({} matching lines total)",
                    offset, total_matching_lines
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
                .unwrap_or(default_opts.max_output_bytes.min(DEFAULT_MAX_BYTES)),
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
            "Read {} ({} lines, {})",
            params.path,
            total_matching_lines,
            format_size(raw_output.len())
        );
        Ok(CallToolResult::success(vec![
            Content::text(output).with_audience(vec![Role::Assistant]),
            Content::text(summary).with_audience(vec![Role::User]),
        ]))
    }

    async fn write_file_impl(&self, params: WriteFileParams) -> Result<CallToolResult, ErrorData> {
        let roots = self.roots.read().await;
        let path = validate_write_path(&params.path, &roots).map_err(invalid_params)?;
        drop(roots);

        if params.content.len() > WRITE_MAX_BYTES {
            return tool_error(format!(
                "Content too large ({}, max {})",
                format_size(params.content.len()),
                format_size(WRITE_MAX_BYTES)
            ));
        }

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| {
                internal_error(format!(
                    "failed to create directories for '{}': {err}",
                    params.path
                ))
            })?;
        }

        std::fs::write(&path, &params.content)
            .map_err(|err| internal_error(format!("failed to write '{}': {err}", params.path)))?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Wrote {} ({} lines) to {}",
            format_size(params.content.len()),
            params.content.lines().count(),
            params.path
        ))]))
    }

    async fn edit_file_impl(&self, params: EditFileParams) -> Result<CallToolResult, ErrorData> {
        let roots = self.roots.read().await;
        let path = validate_path(&params.path, &roots).map_err(invalid_params)?;
        drop(roots);

        let content = std::fs::read_to_string(&path)
            .map_err(|err| internal_error(format!("failed to read '{}': {err}", params.path)))?;

        if content.len() > WRITE_MAX_BYTES {
            return tool_error(format!(
                "File too large for editing ({}, max {})",
                format_size(content.len()),
                format_size(WRITE_MAX_BYTES)
            ));
        }

        if params.old_text.is_empty() {
            return tool_error("old_text must not be empty".to_string());
        }

        let replace_all = params.replace_all.unwrap_or(false);
        let match_count = content.matches(&params.old_text).count();

        if match_count == 0 {
            return tool_error(
                "old_text not found in file. Ensure the text matches exactly including whitespace and indentation."
                    .to_string(),
            );
        }

        if !replace_all && match_count > 1 {
            return tool_error(format!(
                "Found {} matches for old_text. Provide more context or set replace_all=true.",
                match_count
            ));
        }

        let replacements = if replace_all { match_count } else { 1 };
        let size_delta = replacements * params.new_text.len().saturating_sub(params.old_text.len());
        let projected_size = content.len() + size_delta;
        if projected_size > WRITE_MAX_BYTES {
            return tool_error(format!(
                "Replacement would produce a file too large ({}, max {})",
                format_size(projected_size),
                format_size(WRITE_MAX_BYTES)
            ));
        }

        let new_content = if replace_all {
            content.replace(&params.old_text, &params.new_text)
        } else {
            content.replacen(&params.old_text, &params.new_text, 1)
        };

        std::fs::write(&path, new_content)
            .map_err(|err| internal_error(format!("failed to write '{}': {err}", params.path)))?;
        Ok(CallToolResult::success(vec![Content::text(format!(
            "Edited {} ({} replacement{})",
            params.path,
            replacements,
            if replacements == 1 { "" } else { "s" }
        ))]))
    }

    async fn list_directory_impl(
        &self,
        params: ListDirectoryParams,
    ) -> Result<CallToolResult, ErrorData> {
        let roots = self.roots.read().await;
        let path = validate_path(&params.path, &roots).map_err(invalid_params)?;
        drop(roots);

        let metadata = std::fs::metadata(&path)
            .map_err(|err| internal_error(format!("cannot access '{}': {err}", params.path)))?;

        if !metadata.is_dir() {
            return tool_error(format!(
                "'{}' is not a directory. Use read_file for files.",
                params.path
            ));
        }

        let recursive = params.recursive.unwrap_or(false);
        let mut entries = Vec::new();
        let mut scan_count = 0usize;

        if recursive {
            walk_dir_recursive(&path, &path, &mut entries, &mut scan_count);
        } else {
            walk_dir_flat(&path, &mut entries, &mut scan_count);
        }

        entries.sort();
        let limit_reached = entries.len() > DEFAULT_LS_LIMIT;
        entries.truncate(DEFAULT_LS_LIMIT);

        if entries.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "{} (empty directory)",
                params.path
            ))]));
        }

        let entry_count = entries.len();
        let mut output = entries.join("\n");
        if limit_reached {
            let _ = write!(
                output,
                "\n\n[Truncated at {} entries. Use find_files for targeted search.]",
                DEFAULT_LS_LIMIT
            );
        }
        if scan_count >= LS_SCAN_HARD_LIMIT {
            let _ = write!(
                output,
                "\n[Scan stopped at {} entries to prevent excessive I/O.]",
                LS_SCAN_HARD_LIMIT
            );
        }

        let summary = format!("Listed {} entries in {}", entry_count, params.path);
        Ok(CallToolResult::success(vec![
            Content::text(output).with_audience(vec![Role::Assistant]),
            Content::text(summary).with_audience(vec![Role::User]),
        ]))
    }

    async fn search_files_impl(
        &self,
        params: SearchFilesParams,
    ) -> Result<CallToolResult, ErrorData> {
        let roots = self.roots.read().await;
        let search_path = match params.path.as_deref() {
            Some(path) => validate_path(path, &roots).map_err(invalid_params)?,
            None => default_search_path(&roots),
        };
        drop(roots);

        let pattern = if params.ignore_case.unwrap_or(false) {
            format!("(?i){}", params.pattern)
        } else {
            params.pattern.clone()
        };

        let regex = Regex::new(&pattern).map_err(|err| {
            ErrorData::invalid_params(format!("invalid regex '{}': {err}", params.pattern), None)
        })?;

        let include_glob = match params.include.as_deref() {
            Some(pattern) => Some(glob::Pattern::new(pattern).map_err(|err| {
                ErrorData::invalid_params(format!("invalid include glob '{pattern}': {err}"), None)
            })?),
            None => None,
        };

        let context_lines = params.context_lines.unwrap_or(0);
        let max_results = params.max_results.unwrap_or(DEFAULT_GREP_LIMIT);
        let mut results = Vec::new();
        search_recursive(
            &search_path,
            &search_path,
            &regex,
            context_lines,
            max_results + 1,
            include_glob.as_ref(),
            &mut results,
        );

        if results.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No matches found".to_string(),
            )]));
        }

        let limit_reached = results.len() > max_results;
        results.truncate(max_results);
        let match_count = results.len();

        let raw_output = results.join("\n");
        let truncated_output = truncate_output(&raw_output, &TruncateOpts::default());
        let mut notices = Vec::new();
        if limit_reached {
            notices.push(format!(
                "results limited to {} matches. Refine the pattern for more specific results",
                max_results
            ));
        }
        if truncated_output != raw_output {
            notices.push(format!(
                "output truncated from {} to {}. Use max_results to increase the limit",
                format_size(raw_output.len()),
                format_size(truncated_output.len())
            ));
        }

        let mut output = truncated_output;
        if !notices.is_empty() {
            let _ = write!(output, "\n\n[{}]", notices.join(". "));
        }

        let summary = format!(
            "Found {} matches in {}",
            match_count,
            params.path.as_deref().unwrap_or("workspace")
        );
        Ok(CallToolResult::success(vec![
            Content::text(output).with_audience(vec![Role::Assistant]),
            Content::text(summary).with_audience(vec![Role::User]),
        ]))
    }

    async fn find_files_impl(&self, params: FindFilesParams) -> Result<CallToolResult, ErrorData> {
        let roots = self.roots.read().await;
        let search_path = match params.path.as_deref() {
            Some(path) => validate_path(path, &roots).map_err(invalid_params)?,
            None => default_search_path(&roots),
        };
        drop(roots);

        let max_results = params.max_results.unwrap_or(DEFAULT_FIND_LIMIT);
        if params.pattern.contains("..") {
            return Err(ErrorData::invalid_params(
                "glob pattern must not contain '..' path components",
                None,
            ));
        }
        let escaped_base = glob::Pattern::escape(&search_path.display().to_string());
        let full_pattern = format!("{escaped_base}/{}", params.pattern);
        let glob_results = glob::glob(&full_pattern).map_err(|err| {
            ErrorData::invalid_params(format!("invalid glob pattern: {err}"), None)
        })?;

        let mut paths = Vec::new();
        for entry in glob_results {
            if paths.len() > max_results {
                break;
            }
            if let Ok(path) = entry {
                let relative = path
                    .strip_prefix(&search_path)
                    .unwrap_or(&path)
                    .display()
                    .to_string();
                paths.push(relative);
            }
        }

        if paths.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No files found matching pattern".to_string(),
            )]));
        }

        paths.sort();
        let limit_reached = paths.len() > max_results;
        paths.truncate(max_results);
        let path_count = paths.len();

        let mut output = paths.join("\n");
        if limit_reached {
            let _ = write!(
                output,
                "\n\n[Truncated at {} results. Use a more specific pattern.]",
                max_results
            );
        }

        let summary = format!("Found {} matches", path_count);
        Ok(CallToolResult::success(vec![
            Content::text(output).with_audience(vec![Role::Assistant]),
            Content::text(summary).with_audience(vec![Role::User]),
        ]))
    }
}

impl ServerHandler for FsServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "harnx-mcp-fs",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Filesystem MCP server with read, write, edit, listing, grep, and glob tools.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let read_only = ToolAnnotations::new().read_only(true);
        let tools = vec![
                Tool::new("read", "Read a text file with line numbers, pagination, grep filtering, and smart truncation.", Map::new())
                    .with_input_schema::<ReadFileParams>()
                    .annotate(read_only.clone()),
                Tool::new("write", "Write or create a file, replacing its contents.", Map::new())
                    .with_input_schema::<WriteFileParams>(),
                Tool::new("edit", "Replace exact text within an existing file.", Map::new())
                    .with_input_schema::<EditFileParams>(),
                Tool::new("ls", "List directory contents, optionally recursively.", Map::new())
                    .with_input_schema::<ListDirectoryParams>()
                    .annotate(read_only.clone()),
                Tool::new("grep", "Search file contents with regex and optional context lines.", Map::new())
                    .with_input_schema::<SearchFilesParams>()
                    .annotate(read_only.clone()),
                Tool::new("find", "Find files by glob pattern.", Map::new())
                    .with_input_schema::<FindFilesParams>()
                    .annotate(read_only),
            ];

        Ok(ListToolsResult {
            meta: None,
            tools,
            next_cursor: None,
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Err(err) = self.ensure_roots_initialized(&context.peer).await {
            eprintln!("harnx-mcp-fs: failed to initialize roots: {}", err.message);
        }

        match request.name.as_ref() {
            "read" => {
                let params = parse_arguments::<ReadFileParams>(request.arguments)?;
                self.read_file_impl(params).await
            }
            "write" => {
                let params = parse_arguments::<WriteFileParams>(request.arguments)?;
                self.write_file_impl(params).await
            }
            "edit" => {
                let params = parse_arguments::<EditFileParams>(request.arguments)?;
                self.edit_file_impl(params).await
            }
            "ls" => {
                let params = parse_arguments::<ListDirectoryParams>(request.arguments)?;
                self.list_directory_impl(params).await
            }
            "grep" => {
                let params = parse_arguments::<SearchFilesParams>(request.arguments)?;
                self.search_files_impl(params).await
            }
            "find" => {
                let params = parse_arguments::<FindFilesParams>(request.arguments)?;
                self.find_files_impl(params).await
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
                    eprintln!("harnx-mcp-fs: failed to refresh roots: {}", err.message);
                }
            });
        }
    }
}

const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "__pycache__",
    ".next",
    "dist",
    "build",
    ".svn",
    ".hg",
    ".venv",
    "venv",
];

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

fn default_search_path(roots: &[PathBuf]) -> PathBuf {
    roots
        .first()
        .cloned()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
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

fn walk_dir_flat(dir: &Path, entries: &mut Vec<String>, scan_count: &mut usize) {
    let read_dir = match std::fs::read_dir(dir) {
        Ok(read_dir) => read_dir,
        Err(_) => return,
    };

    for entry in read_dir {
        if *scan_count >= LS_SCAN_HARD_LIMIT {
            break;
        }
        *scan_count += 1;

        if let Ok(entry) = entry {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Ok(file_type) = entry.file_type() {
                if file_type.is_dir() {
                    entries.push(format!("{name}/"));
                } else {
                    entries.push(name);
                }
            }
        }
    }
}

fn walk_dir_recursive(base: &Path, dir: &Path, entries: &mut Vec<String>, scan_count: &mut usize) {
    if *scan_count >= LS_SCAN_HARD_LIMIT {
        return;
    }

    let read_dir = match std::fs::read_dir(dir) {
        Ok(read_dir) => read_dir,
        Err(_) => return,
    };

    for entry in read_dir {
        if *scan_count >= LS_SCAN_HARD_LIMIT {
            break;
        }
        *scan_count += 1;

        if let Ok(entry) = entry {
            let path = entry.path();
            let relative = path
                .strip_prefix(base)
                .unwrap_or(&path)
                .display()
                .to_string();

            if let Ok(file_type) = entry.file_type() {
                if file_type.is_dir() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if !SKIP_DIRS.contains(&name.as_str()) {
                        entries.push(format!("{relative}/"));
                        walk_dir_recursive(base, &path, entries, scan_count);
                    }
                } else {
                    entries.push(relative);
                }
            }
        }
    }
}

fn search_recursive(
    base: &Path,
    dir: &Path,
    regex: &Regex,
    context_lines: usize,
    max_results: usize,
    include_glob: Option<&glob::Pattern>,
    results: &mut Vec<String>,
) {
    if results.len() >= max_results {
        return;
    }

    let read_dir = match std::fs::read_dir(dir) {
        Ok(read_dir) => read_dir,
        Err(_) => return,
    };

    for entry in read_dir {
        if results.len() >= max_results {
            return;
        }

        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };

        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }

        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => continue,
        };

        if file_type.is_dir() {
            if !SKIP_DIRS.contains(&name.as_str()) {
                search_recursive(
                    base,
                    &path,
                    regex,
                    context_lines,
                    max_results,
                    include_glob,
                    results,
                );
            }
            continue;
        }

        if !file_type.is_file() {
            continue;
        }

        if let Some(glob) = include_glob {
            if !glob.matches(&name) {
                continue;
            }
        }

        search_file(base, &path, regex, context_lines, max_results, results);
    }
}

fn search_file(
    base: &Path,
    file: &Path,
    regex: &Regex,
    context_lines: usize,
    max_results: usize,
    results: &mut Vec<String>,
) {
    if results.len() >= max_results {
        return;
    }

    let _metadata = match std::fs::metadata(file) {
        Ok(metadata) if metadata.len() <= SEARCH_FILE_MAX_BYTES => metadata,
        _ => return,
    };

    let content = match std::fs::read(file) {
        Ok(content) => content,
        Err(_) => return,
    };

    if is_binary_content(&content) {
        return;
    }

    let text = String::from_utf8_lossy(&content);
    let lines = text.lines().collect::<Vec<_>>();
    let relative = file
        .strip_prefix(base)
        .unwrap_or(file)
        .display()
        .to_string();

    let mut match_indices = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if results.len() + match_indices.len() >= max_results {
            break;
        }
        if let Ok(true) = regex.is_match(line) {
            match_indices.push(index);
        }
    }

    if match_indices.is_empty() {
        return;
    }

    let mut blocks = Vec::<(usize, usize)>::new();
    for &index in &match_indices {
        let start = index.saturating_sub(context_lines);
        let end = (index + context_lines).min(lines.len().saturating_sub(1));
        if let Some(last) = blocks.last_mut() {
            if start <= last.1 + 1 {
                last.1 = last.1.max(end);
                continue;
            }
        }
        blocks.push((start, end));
    }

    for (block_index, (start, end)) in blocks.iter().enumerate() {
        if block_index > 0 {
            results.push("--".to_string());
        }
        for (index, line_content) in lines[*start..=*end].iter().enumerate() {
            let abs_index = start + index;
            let line = truncate_line(line_content, GREP_MAX_LINE_LENGTH);
            let line_number = abs_index + 1;
            let sep = if match_indices.contains(&abs_index) {
                ":"
            } else {
                "-"
            };
            results.push(format!(
                "{}{}{}{} {}",
                relative, sep, line_number, sep, line
            ));
            if results.len() >= max_results {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use harnx_mcp::safety::path_to_file_uri;
    use rmcp::handler::client::ClientHandler;
    use rmcp::model::{
        CallToolRequestParams, ClientCapabilities, InitializeRequestParams, ListRootsResult, Root,
    };
    use rmcp::service::{
        serve_client, serve_server, RequestContext, RoleClient, RoleServer, RunningService,
    };
    use std::path::Path;
    use tokio::io::duplex;
    use uuid::Uuid;

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("harnx-mcp-fs-test-{}", Uuid::new_v4()));
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
    struct TestClientHandler {
        roots: Vec<PathBuf>,
    }

    impl TestClientHandler {
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
                    .map(|root| Root::new(path_to_file_uri(&root.canonicalize().unwrap())))
                    .collect(),
            ))
        }
    }

    struct TestConnection {
        _server_service: RunningService<RoleServer, FsServer>,
        client_service: RunningService<RoleClient, TestClientHandler>,
    }

    async fn connect_server(server: FsServer, roots: Vec<PathBuf>) -> TestConnection {
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
            .find_map(|content| content.raw.as_text().map(|text| text.text.clone()))
            .unwrap()
    }

    fn tool_args(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[tokio::test]
    async fn test_fs_server_list_tools() {
        let temp_dir = TestDir::new();
        let TestConnection {
            _server_service,
            client_service,
        } = connect_server(
            FsServer::new(vec![temp_dir.path().to_path_buf()]),
            vec![temp_dir.path().to_path_buf()],
        )
        .await;
        let peer = client_service.peer().clone();
        let _client_task = tokio::spawn(async move {
            let _ = client_service.waiting().await;
        });

        let tools = peer.list_tools(Default::default()).await.unwrap();
        let names = tools
            .tools
            .iter()
            .map(|tool| tool.name.to_string())
            .collect::<Vec<_>>();

        assert_eq!(names, vec!["read", "write", "edit", "ls", "grep", "find"]);
    }

    #[tokio::test]
    async fn test_fs_server_read_file() {
        let temp_dir = TestDir::new();
        let file_path = temp_dir.path().join("notes.txt");
        std::fs::write(&file_path, "alpha\nbeta\n").unwrap();

        let TestConnection {
            _server_service,
            client_service,
        } = connect_server(
            FsServer::new(vec![temp_dir.path().to_path_buf()]),
            vec![temp_dir.path().to_path_buf()],
        )
        .await;
        let peer = client_service.peer().clone();
        let _client_task = tokio::spawn(async move {
            let _ = client_service.waiting().await;
        });

        let result = peer
            .call_tool(CallToolRequestParams::new("read").with_arguments(tool_args(
                serde_json::json!({
                    "path": file_path.to_string_lossy().to_string()
                }),
            )))
            .await
            .unwrap();

        let text = text_content(&result);
        assert_eq!(result.is_error, Some(false));
        assert!(text.contains("1: alpha"));
        assert!(text.contains("2: beta"));
    }

    #[tokio::test]
    async fn test_fs_server_write_and_read() {
        let temp_dir = TestDir::new();
        let file_path = temp_dir.path().join("written.txt");

        let TestConnection {
            _server_service,
            client_service,
        } = connect_server(
            FsServer::new(vec![temp_dir.path().to_path_buf()]),
            vec![temp_dir.path().to_path_buf()],
        )
        .await;
        let peer = client_service.peer().clone();
        let _client_task = tokio::spawn(async move {
            let _ = client_service.waiting().await;
        });

        let write_result = peer
            .call_tool(
                CallToolRequestParams::new("write").with_arguments(tool_args(serde_json::json!({
                    "path": file_path.to_string_lossy().to_string(),
                    "content": "hello\nworld\n"
                }))),
            )
            .await
            .unwrap();
        assert_eq!(write_result.is_error, Some(false));

        let read_result = peer
            .call_tool(CallToolRequestParams::new("read").with_arguments(tool_args(
                serde_json::json!({
                    "path": file_path.to_string_lossy().to_string()
                }),
            )))
            .await
            .unwrap();

        let text = text_content(&read_result);
        assert!(text.contains("1: hello"));
        assert!(text.contains("2: world"));
    }

    #[tokio::test]
    async fn test_fs_server_edit_file() {
        let temp_dir = TestDir::new();
        let file_path = temp_dir.path().join("edit.txt");
        std::fs::write(&file_path, "old value\n").unwrap();

        let TestConnection {
            _server_service,
            client_service,
        } = connect_server(
            FsServer::new(vec![temp_dir.path().to_path_buf()]),
            vec![temp_dir.path().to_path_buf()],
        )
        .await;
        let peer = client_service.peer().clone();
        let _client_task = tokio::spawn(async move {
            let _ = client_service.waiting().await;
        });

        let edit_result = peer
            .call_tool(CallToolRequestParams::new("edit").with_arguments(tool_args(
                serde_json::json!({
                    "path": file_path.to_string_lossy().to_string(),
                    "old_text": "old value",
                    "new_text": "new value"
                }),
            )))
            .await
            .unwrap();
        assert_eq!(edit_result.is_error, Some(false));
        assert_eq!(std::fs::read_to_string(&file_path).unwrap(), "new value\n");
    }

    #[tokio::test]
    async fn test_read_file_with_offset_limit() {
        let temp_dir = TestDir::new();
        let file_path = temp_dir.path().join("offset.txt");
        std::fs::write(&file_path, "one\ntwo\nthree\nfour\n").unwrap();
        let server = FsServer::new(vec![temp_dir.path().to_path_buf()]);

        let result = server
            .read_file_impl(ReadFileParams {
                path: file_path.to_string_lossy().to_string(),
                offset: Some(2),
                limit: Some(2),
                tail: None,
                grep: None,
                head_lines: None,
                tail_lines: None,
                max_output_bytes: None,
            })
            .await
            .unwrap();

        let text = text_content(&result);
        assert!(text.contains("2: two"));
        assert!(text.contains("3: three"));
        assert!(text.contains("Use offset=4 to continue"));
    }

    #[tokio::test]
    async fn test_read_file_with_grep() {
        let temp_dir = TestDir::new();
        let file_path = temp_dir.path().join("grep.txt");
        std::fs::write(&file_path, "alpha\nmatch-one\nbeta\nmatch-two\n").unwrap();
        let server = FsServer::new(vec![temp_dir.path().to_path_buf()]);

        let result = server
            .read_file_impl(ReadFileParams {
                path: file_path.to_string_lossy().to_string(),
                offset: None,
                limit: None,
                tail: None,
                grep: Some("match".to_string()),
                head_lines: None,
                tail_lines: None,
                max_output_bytes: None,
            })
            .await
            .unwrap();

        let text = text_content(&result);
        assert!(text.contains("2: match-one"));
        assert!(text.contains("4: match-two"));
        assert!(!text.contains("1: alpha"));
    }

    #[tokio::test]
    async fn test_read_file_with_tail() {
        let temp_dir = TestDir::new();
        let file_path = temp_dir.path().join("tail.txt");
        std::fs::write(&file_path, "one\ntwo\nthree\nfour\n").unwrap();
        let server = FsServer::new(vec![temp_dir.path().to_path_buf()]);

        let result = server
            .read_file_impl(ReadFileParams {
                path: file_path.to_string_lossy().to_string(),
                offset: None,
                limit: None,
                tail: Some(2),
                grep: None,
                head_lines: None,
                tail_lines: None,
                max_output_bytes: None,
            })
            .await
            .unwrap();

        let text = text_content(&result);
        assert!(text.contains("3: three"));
        assert!(text.contains("4: four"));
        assert!(text.contains("showing last 2 of 4 matching lines"));
    }

    #[tokio::test]
    async fn test_read_file_binary_detection() {
        let temp_dir = TestDir::new();
        let file_path = temp_dir.path().join("binary.bin");
        std::fs::write(&file_path, b"hello\0world").unwrap();
        let server = FsServer::new(vec![temp_dir.path().to_path_buf()]);

        let result = server
            .read_file_impl(ReadFileParams {
                path: file_path.to_string_lossy().to_string(),
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

        assert_eq!(result.is_error, Some(true));
        assert!(text_content(&result).contains("appears to be a binary file"));
    }

    #[tokio::test]
    async fn test_edit_file_unique_match() {
        let temp_dir = TestDir::new();
        let file_path = temp_dir.path().join("unique.txt");
        std::fs::write(&file_path, "alpha\nbeta\n").unwrap();
        let server = FsServer::new(vec![temp_dir.path().to_path_buf()]);

        let result = server
            .edit_file_impl(EditFileParams {
                path: file_path.to_string_lossy().to_string(),
                old_text: "beta".to_string(),
                new_text: "gamma".to_string(),
                replace_all: Some(false),
            })
            .await
            .unwrap();

        assert_eq!(result.is_error, Some(false));
        assert_eq!(
            std::fs::read_to_string(&file_path).unwrap(),
            "alpha\ngamma\n"
        );
    }

    #[tokio::test]
    async fn test_edit_file_multiple_matches() {
        let temp_dir = TestDir::new();
        let file_path = temp_dir.path().join("multiple.txt");
        std::fs::write(&file_path, "value\nvalue\n").unwrap();
        let server = FsServer::new(vec![temp_dir.path().to_path_buf()]);

        let result = server
            .edit_file_impl(EditFileParams {
                path: file_path.to_string_lossy().to_string(),
                old_text: "value".to_string(),
                new_text: "updated".to_string(),
                replace_all: Some(false),
            })
            .await
            .unwrap();

        assert_eq!(result.is_error, Some(true));
        assert!(text_content(&result).contains("Found 2 matches"));
    }

    #[tokio::test]
    async fn test_list_directory_flat() {
        let temp_dir = TestDir::new();
        std::fs::create_dir_all(temp_dir.path().join("nested")).unwrap();
        std::fs::write(temp_dir.path().join("root.txt"), "root").unwrap();
        std::fs::write(temp_dir.path().join("nested").join("child.txt"), "child").unwrap();
        let server = FsServer::new(vec![temp_dir.path().to_path_buf()]);

        let result = server
            .list_directory_impl(ListDirectoryParams {
                path: temp_dir.path().to_string_lossy().to_string(),
                recursive: Some(false),
            })
            .await
            .unwrap();

        let text = text_content(&result);
        assert!(text.contains("nested/"));
        assert!(text.contains("root.txt"));
        assert!(!text.contains("child.txt"));
    }

    #[tokio::test]
    async fn test_search_files_basic() {
        let temp_dir = TestDir::new();
        std::fs::write(temp_dir.path().join("one.txt"), "alpha\nneedle\nomega\n").unwrap();
        std::fs::write(temp_dir.path().join("two.txt"), "nothing here\n").unwrap();
        let server = FsServer::new(vec![temp_dir.path().to_path_buf()]);

        let result = server
            .search_files_impl(SearchFilesParams {
                pattern: "needle".to_string(),
                path: Some(temp_dir.path().to_string_lossy().to_string()),
                include: Some("*.txt".to_string()),
                context_lines: Some(0),
                ignore_case: Some(false),
                max_results: Some(10),
            })
            .await
            .unwrap();

        let text = text_content(&result);
        assert!(text.contains("one.txt:2: needle"));
        assert!(!text.contains("two.txt"));
    }
}

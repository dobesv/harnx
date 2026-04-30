use crate::summary::{
    apply_search_notices, find_summary, ls_summary, render_read_result, search_summary,
    SearchTruncation,
};

use harnx_mcp::safety::{
    file_uri_to_path, format_size, is_binary_content, sanitize_output_text, truncate_line,
    validate_path, validate_write_path, DEFAULT_FIND_LIMIT, DEFAULT_GREP_LIMIT, DEFAULT_LS_LIMIT,
    DEFAULT_MAX_LINES, GREP_MAX_LINE_LENGTH, LS_SCAN_HARD_LIMIT, READ_MAX_FILE_BYTES,
    SEARCH_FILE_MAX_BYTES, WRITE_MAX_BYTES,
};
use harnx_mcp_history::HistoryManager;

use fancy_regex::Regex;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, ErrorData, Implementation, ListToolsResult,
    Meta, PaginatedRequestParams, Role, ServerCapabilities, ServerInfo, Tool, ToolAnnotations,
};
use rmcp::schemars::{generate::SchemaGenerator, JsonSchema, Schema};
use rmcp::service::{NotificationContext, RequestContext, RoleServer};
use rmcp::ServerHandler;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Map, Value};
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

#[derive(Debug, Deserialize)]
pub struct RollbackParams {
    pub commit_id: String,
    pub repo_path: String,
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

#[derive(Clone)]
pub struct FsServer {
    roots: Arc<RwLock<Vec<PathBuf>>>,
    roots_initialized: Arc<AtomicBool>,
    history: Arc<HistoryManager>,
}

impl FsServer {
    pub fn new(initial_roots: Vec<PathBuf>) -> Self {
        Self {
            roots: Arc::new(RwLock::new(initial_roots.clone())),
            roots_initialized: Arc::new(AtomicBool::new(false)),
            history: Arc::new(HistoryManager::new(&initial_roots)),
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

        let (shown_line_start, shown_line_end) = if let Some(tail_count) = params.tail {
            if tail_count == 0 {
                return tool_error("tail must be at least 1".to_string());
            }
            if tail_count < total_matching_lines {
                notices.push(format!(
                    "showing last {} of {} matching lines",
                    tail_count, total_matching_lines
                ));
            }
            let start = total_matching_lines.saturating_sub(tail_count);
            numbered_lines = numbered_lines.into_iter().skip(start).collect();
            (start + 1, total_matching_lines)
        } else {
            let offset = params.offset.unwrap_or(1).max(1);
            let limit = params.limit.unwrap_or(DEFAULT_MAX_LINES);

            if offset > total_matching_lines {
                return tool_error(format!(
                    "Offset {} is beyond end of result set ({} matching lines total)",
                    offset, total_matching_lines
                ));
            }

            if limit == 0 {
                return tool_error("limit must be at least 1".to_string());
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
            (offset, end)
        };

        render_read_result(
            &params,
            numbered_lines,
            total_matching_lines,
            shown_line_start,
            shown_line_end,
            bytes.len(),
            notices,
        )
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

        // HISTORY: before snapshot
        let before_snap = self
            .history
            .snapshot_file(&path, "before write_file")
            .await
            .map_err(|e| {
                log::warn!("history before-snapshot failed: {e}");
            })
            .ok();

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

        // HISTORY: after snapshot + diff
        let after_snap_result = if let Some(before) = before_snap {
            match self.history.snapshot_file(&path, "after write_file").await {
                Ok(after) => {
                    let diff = if let Some(repo_dir) =
                        harnx_mcp_history::discover::find_repo_for_path(&path)
                    {
                        self.history
                            .diff_commits(&repo_dir, before, after)
                            .await
                            .unwrap_or_default()
                    } else {
                        String::new()
                    };
                    Some((after, diff))
                }
                Err(e) => {
                    log::warn!("history after-snapshot failed: {e}");
                    None
                }
            }
        } else {
            None
        };

        let mut contents = vec![Content::text(format!(
            "Wrote {} ({} lines) to {}",
            format_size(params.content.len()),
            params.content.lines().count(),
            params.path
        ))];
        if let Some((_after_id, diff_content)) = after_snap_result {
            if !diff_content.is_empty() {
                contents.push(Content::text(diff_content));
            }
        }
        Ok(CallToolResult::success(contents))
    }

    async fn edit_file_impl(&self, params: EditFileParams) -> Result<CallToolResult, ErrorData> {
        let roots = self.roots.read().await;
        let path = validate_path(&params.path, &roots).map_err(invalid_params)?;
        drop(roots);

        // HISTORY: before snapshot
        let before_snap = self
            .history
            .snapshot_file(&path, "before edit_file")
            .await
            .map_err(|e| {
                log::warn!("history before-snapshot failed: {e}");
            })
            .ok();

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

        // HISTORY: after snapshot + diff
        let after_snap_result = if let Some(before) = before_snap {
            match self.history.snapshot_file(&path, "after edit_file").await {
                Ok(after) => {
                    let diff = if let Some(repo_dir) =
                        harnx_mcp_history::discover::find_repo_for_path(&path)
                    {
                        self.history
                            .diff_commits(&repo_dir, before, after)
                            .await
                            .unwrap_or_default()
                    } else {
                        String::new()
                    };
                    Some((after, diff))
                }
                Err(e) => {
                    log::warn!("history after-snapshot failed: {e}");
                    None
                }
            }
        } else {
            None
        };

        let mut contents = vec![Content::text(format!(
            "Edited {} ({} replacement{})",
            params.path,
            replacements,
            if replacements == 1 { "" } else { "s" }
        ))];
        if let Some((_after_id, diff_content)) = after_snap_result {
            if !diff_content.is_empty() {
                contents.push(Content::text(diff_content));
            }
        }
        Ok(CallToolResult::success(contents))
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

        let summary = ls_summary(&params.path, entry_count, scan_count, limit_reached);
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
        let raw_bytes = raw_output.len();
        let (output, byte_truncated) = apply_search_notices(raw_output, limit_reached, max_results);

        let search_location = params.path.as_deref().unwrap_or("workspace");
        let summary = search_summary(
            search_location,
            &SearchTruncation {
                match_count,
                max_results,
                limit_reached,
                output_bytes: output.len(),
                raw_bytes,
                byte_truncated,
            },
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
        // The glob crate always expects '/' as the path separator, even on Windows,
        // so normalize the base path before escaping (Path::display yields '\' on Windows).
        let mut base_str = search_path.display().to_string();
        if std::path::MAIN_SEPARATOR != '/' {
            base_str = base_str.replace(std::path::MAIN_SEPARATOR, "/");
        }
        let escaped_base = glob::Pattern::escape(&base_str);
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

        let summary = find_summary(path_count, max_results, limit_reached);
        Ok(CallToolResult::success(vec![
            Content::text(output).with_audience(vec![Role::Assistant]),
            Content::text(summary).with_audience(vec![Role::User]),
        ]))
    }
}

impl FsServer {
    async fn rollback_file_impl(
        &self,
        params: RollbackParams,
    ) -> Result<CallToolResult, ErrorData> {
        let roots = self.roots.read().await;
        let path = validate_path(&params.repo_path, &roots).map_err(invalid_params)?;
        drop(roots);

        let commit_id = gix::ObjectId::from_hex(params.commit_id.as_bytes())
            .map_err(|e| ErrorData::invalid_params(format!("invalid commit_id: {e}"), None))?;

        let repo_dir = harnx_mcp_history::discover::find_repo_for_path(&path).ok_or_else(|| {
            ErrorData::invalid_params("path is not inside a git repository".to_string(), None)
        })?;

        let new_commit_id = self
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
}

fn make_tool_meta(call_template: &str) -> Meta {
    Meta(
        json!({
            "call_template": call_template,
            "result_template": "{{ result.content[0].text | default('') }}"
        })
        .as_object()
        .unwrap()
        .clone(),
    )
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
                    .annotate(read_only.clone())
                    .with_meta(make_tool_meta("**read** `{{ args.path }}`")),
                Tool::new("write", "Write or create a file, replacing its contents.", Map::new())
                    .with_input_schema::<WriteFileParams>()
                    .with_meta(make_tool_meta("**write** `{{ args.path }}` ({{ args.content | length }} chars)")),
                Tool::new("edit", "Replace exact text within an existing file.", Map::new())
                    .with_input_schema::<EditFileParams>()
                    .with_meta(make_tool_meta("**edit** `{{ args.path }}`")),
                Tool::new("ls", "List directory contents, optionally recursively.", Map::new())
                    .with_input_schema::<ListDirectoryParams>()
                    .annotate(read_only.clone())
                    .with_meta(make_tool_meta("**ls** `{{ args.path }}`{% if args.recursive %} (recursive){% endif %}")),
                Tool::new("grep", "Search file contents with regex and optional context lines.", Map::new())
                    .with_input_schema::<SearchFilesParams>()
                    .annotate(read_only.clone())
                    .with_meta(make_tool_meta("**grep** `{{ args.pattern }}`")),
                Tool::new("find", "Find files by glob pattern.", Map::new())
                    .with_input_schema::<FindFilesParams>()
                    .annotate(read_only.clone())
                    .with_meta(make_tool_meta("**find** `{{ args.pattern }}`")),
                Tool::new("rollback_file", "Restore a repository to a prior harnx history snapshot. Pass the commit SHA from the 'commit <sha>' line at the top of a prior tool response's diff as the commit_id parameter.", Map::new())
                    .with_input_schema::<RollbackParams>()
                    .with_meta(make_tool_meta("**rollback_file** to `{{ args.commit_id | truncate(8, end='') }}`")),
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

    fn make_server(dir: &std::path::Path) -> FsServer {
        FsServer::new(vec![dir.to_path_buf()])
    }

    fn user_summary(result: &CallToolResult) -> String {
        result
            .content
            .iter()
            .filter(|content| {
                content
                    .audience()
                    .map(|a| a.contains(&Role::User))
                    .unwrap_or(false)
            })
            .find_map(|content| content.raw.as_text().map(|text| text.text.clone()))
            .unwrap_or_default()
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
            make_server(temp_dir.path()),
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

        assert_eq!(
            names,
            vec![
                "read",
                "write",
                "edit",
                "ls",
                "grep",
                "find",
                "rollback_file"
            ]
        );
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
            make_server(temp_dir.path()),
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
            make_server(temp_dir.path()),
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
            make_server(temp_dir.path()),
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
        let server = make_server(temp_dir.path());

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
        let server = make_server(temp_dir.path());

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
        let server = make_server(temp_dir.path());

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
        let server = make_server(temp_dir.path());

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

    // ── truncation-in-user-summary tests (issue #144) ──────────────────────

    #[tokio::test]
    async fn test_read_file_summary_limited_on_pagination() {
        // offset=1 limit=2 on a 4-line file → shows lines 1–2, more remain.
        // Summary must show the slice range and byte counts.
        let temp_dir = TestDir::new();
        let file_path = temp_dir.path().join("paginated.txt");
        std::fs::write(&file_path, "one\ntwo\nthree\nfour\n").unwrap();
        let server = FsServer::new(vec![temp_dir.path().to_path_buf()]);

        let result = server
            .read_file_impl(ReadFileParams {
                path: file_path.to_string_lossy().to_string(),
                offset: Some(1),
                limit: Some(2),
                tail: None,
                grep: None,
                head_lines: None,
                tail_lines: None,
                max_output_bytes: None,
            })
            .await
            .unwrap();

        let summary = user_summary(&result);
        assert!(
            summary.contains("lines 1\u{2013}2 of 4"),
            "expected exact paginated range 'lines 1\u{2013}2 of 4' in summary, got: {summary:?}"
        );
    }

    #[tokio::test]
    async fn test_list_directory_summary_not_limited_when_small() {
        let temp_dir = TestDir::new();
        for i in 0..3 {
            std::fs::write(temp_dir.path().join(format!("f{i}.txt")), "x").unwrap();
        }
        let server = FsServer::new(vec![temp_dir.path().to_path_buf()]);

        let result = server
            .list_directory_impl(ListDirectoryParams {
                path: temp_dir.path().to_string_lossy().to_string(),
                recursive: Some(false),
            })
            .await
            .unwrap();

        let summary = user_summary(&result);
        assert!(
            !summary.contains("limited"),
            "expected no 'limited' for small listing, got: {summary:?}"
        );
        assert!(
            summary.contains("Listed 3 entries"),
            "expected count in summary, got: {summary:?}"
        );
    }

    #[tokio::test]
    async fn test_list_directory_summary_limited_when_over_default_limit() {
        // Create DEFAULT_LS_LIMIT + 1 files to trigger limit_reached.
        // Summary should show "Listed 500 of 501 entries in …".
        let temp_dir = TestDir::new();
        for i in 0..=DEFAULT_LS_LIMIT {
            std::fs::write(temp_dir.path().join(format!("f{i:04}.txt")), "x").unwrap();
        }
        let server = FsServer::new(vec![temp_dir.path().to_path_buf()]);

        let result = server
            .list_directory_impl(ListDirectoryParams {
                path: temp_dir.path().to_string_lossy().to_string(),
                recursive: Some(false),
            })
            .await
            .unwrap();

        let summary = user_summary(&result);
        // Should show "Listed 500 of 501 entries" — capped count + true total.
        assert!(
            summary.contains(&format!(
                "Listed {} of {} entries",
                DEFAULT_LS_LIMIT,
                DEFAULT_LS_LIMIT + 1
            )),
            "expected 'Listed N of M entries' in summary, got: {summary:?}"
        );
    }

    #[tokio::test]
    async fn test_search_files_summary_variants() {
        struct Case {
            files: &'static [(&'static str, &'static str)],
            max_results: usize,
            check: fn(&str),
        }

        let cases: &[Case] = &[
            Case {
                files: &[
                    ("match0.txt", "needle\n"),
                    ("match1.txt", "needle\n"),
                    ("match2.txt", "needle\n"),
                ],
                max_results: 1,
                check: |summary| {
                    assert!(
                        summary.contains("1+"),
                        "expected '1+' in summary when max_results hit, got: {summary:?}"
                    );
                    assert!(
                        summary.contains("showing 1"),
                        "expected 'showing 1' in summary, got: {summary:?}"
                    );
                },
            },
            Case {
                files: &[("one.txt", "needle\n")],
                max_results: 10,
                check: |summary| {
                    assert!(
                        !summary.contains("limited"),
                        "expected no 'limited' when all results returned, got: {summary:?}"
                    );
                },
            },
        ];

        for case in cases {
            let temp_dir = TestDir::new();
            for (name, content) in case.files {
                std::fs::write(temp_dir.path().join(name), content).unwrap();
            }
            let server = FsServer::new(vec![temp_dir.path().to_path_buf()]);

            let result = server
                .search_files_impl(SearchFilesParams {
                    pattern: "needle".to_string(),
                    path: Some(temp_dir.path().to_string_lossy().to_string()),
                    include: None,
                    context_lines: Some(0),
                    ignore_case: Some(false),
                    max_results: Some(case.max_results),
                })
                .await
                .unwrap();

            (case.check)(user_summary(&result).as_str());
        }
    }

    #[tokio::test]
    async fn test_find_files_basic() {
        // Regression: glob pattern must use '/' not MAIN_SEPARATOR —
        // the glob crate expects Unix separators on all platforms.
        let temp_dir = TestDir::new();
        std::fs::write(temp_dir.path().join("hello.txt"), "").unwrap();
        let server = FsServer::new(vec![temp_dir.path().to_path_buf()]);

        let result = server
            .find_files_impl(FindFilesParams {
                pattern: "*.txt".to_string(),
                path: Some(temp_dir.path().to_string_lossy().to_string()),
                max_results: Some(10),
            })
            .await
            .unwrap();

        let text = text_content(&result);
        assert!(
            text.contains("hello.txt"),
            "find_files should locate files on any platform, got: {text:?}"
        );
    }

    #[tokio::test]
    async fn test_find_files_summary_variants() {
        struct Case {
            files: &'static [&'static str],
            max_results: usize,
            check: fn(&str),
        }

        let cases: &[Case] = &[
            Case {
                files: &["file0.txt", "file1.txt", "file2.txt"],
                max_results: 1,
                check: |summary| {
                    assert!(
                        summary.contains("1+"),
                        "expected '1+' in find_files summary when max_results hit, got: {summary:?}"
                    );
                    assert!(
                        summary.contains("showing 1"),
                        "expected 'showing 1' in find_files summary, got: {summary:?}"
                    );
                },
            },
            Case {
                files: &["only.txt"],
                max_results: 10,
                check: |summary| {
                    assert!(
                        !summary.contains("limited"),
                        "expected no 'limited' when all files returned, got: {summary:?}"
                    );
                },
            },
        ];

        for case in cases {
            let temp_dir = TestDir::new();
            for name in case.files {
                std::fs::write(temp_dir.path().join(name), "").unwrap();
            }
            let server = FsServer::new(vec![temp_dir.path().to_path_buf()]);

            let result = server
                .find_files_impl(FindFilesParams {
                    pattern: "*.txt".to_string(),
                    path: Some(temp_dir.path().to_string_lossy().to_string()),
                    max_results: Some(case.max_results),
                })
                .await
                .unwrap();

            (case.check)(user_summary(&result).as_str());
        }
    }
}

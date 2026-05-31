// Auto-split from server.rs for cohesion. See server/mod.rs.
use super::*;

type ReadLinesPage<'a> = (Vec<(usize, &'a str)>, usize, usize);

impl FsServer {
    pub fn new(initial_roots: Vec<PathBuf>) -> Self {
        Self {
            roots: Arc::new(RwLock::new(initial_roots.clone())),
            roots_initialized: Arc::new(AtomicBool::new(false)),
            history: Arc::new(HistoryManager::new(&initial_roots)),
        }
    }

    async fn snapshot_before(&self, path: &Path, label: &str) -> Option<gix::ObjectId> {
        self.history
            .snapshot_file(path, label)
            .await
            .map_err(|e| {
                log::warn!("history before-snapshot failed: {e}");
            })
            .ok()
    }

    async fn snapshot_after_diff(
        &self,
        path: &Path,
        before: Option<gix::ObjectId>,
        label: &str,
    ) -> Option<String> {
        let before = before?;
        let after = match self.history.snapshot_file(path, label).await {
            Ok(after) => after,
            Err(e) => {
                log::warn!("history after-snapshot failed: {e}");
                return None;
            }
        };

        let Some(repo_dir) = harnx_mcp_history::discover::find_repo_for_path(path) else {
            return Some(String::new());
        };

        Some(
            self.history
                .diff_commits(&repo_dir, before, after)
                .await
                .unwrap_or_default(),
        )
    }

    fn mutation_success(message: String, diff: Option<String>) -> CallToolResult {
        let mut contents = vec![Content::text(message)];
        if let Some(diff_content) = diff.filter(|diff| !diff.is_empty()) {
            contents.push(Content::text(diff_content));
        }
        CallToolResult::success(contents)
    }

    fn paginate_read_lines<'a>(
        params: &ReadFileParams,
        mut numbered_lines: Vec<(usize, &'a str)>,
        notices: &mut Vec<String>,
    ) -> Result<ReadLinesPage<'a>, ErrorData> {
        let total_matching_lines = numbered_lines.len();
        if let Some(tail) = params.tail {
            if tail == 0 {
                return Err(ErrorData::internal_error(
                    "tail must be at least 1".to_string(),
                    None,
                ));
            }

            if tail < total_matching_lines {
                notices.push(format!(
                    "showing last {} of {} matching lines",
                    tail, total_matching_lines
                ));
            }

            let start = total_matching_lines.saturating_sub(tail);
            numbered_lines = numbered_lines.into_iter().skip(start).collect();
            return Ok((numbered_lines, start + 1, total_matching_lines));
        }

        let offset = params.offset.unwrap_or(1).max(1);
        let limit = params.limit.unwrap_or(DEFAULT_MAX_LINES);

        if offset > total_matching_lines {
            return Err(ErrorData::internal_error(
                format!(
                    "Offset {} is beyond end of result set ({} matching lines total)",
                    offset, total_matching_lines
                ),
                None,
            ));
        }

        if limit == 0 {
            return Err(ErrorData::internal_error(
                "limit must be at least 1".to_string(),
                None,
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

        Ok((numbered_lines[start..end].to_vec(), offset, end))
    }

    fn build_insert_content(params: &InsertParams, content: &str) -> Result<String, ErrorData> {
        let lines = content.split_inclusive('\n').collect::<Vec<_>>();
        let total_lines = lines.len();
        let append_mode = params.insert_line.is_none();
        let insert_line = params.insert_line.unwrap_or(total_lines);
        if insert_line > total_lines {
            return Err(ErrorData::invalid_params(
                format!(
                    "insert_line {} out of range for file with {} lines",
                    insert_line, total_lines
                ),
                None,
            ));
        }

        if params.column == Some(0) {
            return Err(invalid_params(
                "column is 1-indexed; 0 is invalid (use 1 for start of line or omit for whole-line insert)",
            ));
        }

        if insert_line == 0 {
            return Ok(format!("{}{}", params.insert_text, content));
        }

        if append_mode || params.column.unwrap_or(1) <= 1 {
            return Ok(format!(
                "{}{}{}",
                lines[..insert_line].join(""),
                params.insert_text,
                lines[insert_line..].join("")
            ));
        }

        Self::insert_with_column(params, &lines, insert_line)
    }

    fn insert_with_column(
        params: &InsertParams,
        lines: &[&str],
        insert_line: usize,
    ) -> Result<String, ErrorData> {
        let line = lines[insert_line - 1];
        let (stripped_line, had_newline) = match line.strip_suffix('\n') {
            Some(stripped) => (stripped, true),
            None => (line, false),
        };
        let column = params.column.expect("column validated by caller");
        let insert_index = column - 1;
        if insert_index > stripped_line.len() || !stripped_line.is_char_boundary(insert_index) {
            return Err(ErrorData::internal_error(
                format!(
                    "column {} is not a valid UTF-8 character boundary in line {}",
                    column, insert_line
                ),
                None,
            ));
        }

        let new_line = if had_newline {
            format!(
                "{}{}{}\n",
                &stripped_line[..insert_index],
                params.insert_text,
                &stripped_line[insert_index..]
            )
        } else {
            format!(
                "{}{}{}",
                &stripped_line[..insert_index],
                params.insert_text,
                &stripped_line[insert_index..]
            )
        };

        Ok(format!(
            "{}{}{}",
            lines[..insert_line - 1].join(""),
            new_line,
            lines[insert_line..].join("")
        ))
    }

    fn find_files_pattern(search_path: &Path, pattern: &str) -> String {
        let mut base_str = search_path.display().to_string();
        #[cfg(windows)]
        {
            if let Some(rest) = base_str.strip_prefix(r"\\?\UNC\").map(str::to_owned) {
                base_str = format!(r"\\{rest}");
            } else if let Some(rest) = base_str.strip_prefix(r"\\?\").map(str::to_owned) {
                base_str = rest;
            }
        }
        if std::path::MAIN_SEPARATOR != '/' {
            base_str = base_str.replace(std::path::MAIN_SEPARATOR, "/");
        }
        let escaped_base = glob::Pattern::escape(&base_str);
        format!("{escaped_base}/{pattern}")
    }

    fn collect_found_paths(
        search_path: &Path,
        glob_results: glob::Paths,
        max_results: usize,
    ) -> Vec<String> {
        let mut paths = Vec::new();
        for entry in glob_results {
            if paths.len() > max_results {
                break;
            }
            if let Ok(path) = entry {
                let relative = path
                    .strip_prefix(search_path)
                    .unwrap_or(&path)
                    .display()
                    .to_string();
                paths.push(relative);
            }
        }
        paths
    }

    pub(crate) async fn refresh_roots(
        &self,
        peer: &rmcp::service::Peer<RoleServer>,
    ) -> Result<(), ErrorData> {
        if !peer_supports_roots(peer) {
            // The client never advertised the `roots` capability, so it can't
            // answer a `roots/list` request. Keep the CLI-provided roots and
            // mark initialization done so we don't keep retrying.
            self.roots_initialized.store(true, Ordering::SeqCst);
            return Ok(());
        }

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

    pub(crate) async fn ensure_roots_initialized(
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

    pub(crate) async fn read_file_impl(
        &self,
        params: ReadFileParams,
    ) -> Result<CallToolResult, ErrorData> {
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

        let numbered_lines = all_lines
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

        let (numbered_lines, shown_line_start, shown_line_end) =
            Self::paginate_read_lines(&params, numbered_lines, &mut notices)?;

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

    pub(crate) async fn write_file_impl(
        &self,
        params: WriteFileParams,
    ) -> Result<CallToolResult, ErrorData> {
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

        let before_snap = self.snapshot_before(&path, "before write_file").await;

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

        let diff = self
            .snapshot_after_diff(&path, before_snap, "after write_file")
            .await;

        Ok(Self::mutation_success(
            format!(
                "Wrote {} ({} lines) to {}",
                format_size(params.content.len()),
                params.content.lines().count(),
                params.path
            ),
            diff,
        ))
    }

    pub(crate) async fn insert_impl(
        &self,
        params: InsertParams,
    ) -> Result<CallToolResult, ErrorData> {
        let roots = self.roots.read().await;
        let path = validate_write_path(&params.path, &roots).map_err(invalid_params)?;
        drop(roots);

        let before_snap = self.snapshot_before(&path, "before insert").await;

        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|err| internal_error(format!("failed to read '{}': {err}", params.path)))?;

        if content.len() > WRITE_MAX_BYTES {
            return tool_error(format!(
                "File too large for editing ({}, max {})",
                format_size(content.len()),
                format_size(WRITE_MAX_BYTES)
            ));
        }

        let new_content = match Self::build_insert_content(&params, &content) {
            Ok(new_content) => new_content,
            Err(err) => return tool_error(err.message),
        };

        if new_content.len() > WRITE_MAX_BYTES {
            return tool_error(format!(
                "Insertion would produce a file too large ({}, max {})",
                format_size(new_content.len()),
                format_size(WRITE_MAX_BYTES)
            ));
        }

        std::fs::write(&path, &new_content)
            .map_err(|err| internal_error(format!("failed to write '{}': {err}", params.path)))?;

        let diff = self
            .snapshot_after_diff(&path, before_snap, "after insert")
            .await;

        Ok(Self::mutation_success(
            format!(
                "Inserted {} bytes into {}",
                params.insert_text.len(),
                params.path
            ),
            diff,
        ))
    }

    pub(crate) async fn re_replace_impl(
        &self,
        params: ReReplaceParams,
    ) -> Result<CallToolResult, ErrorData> {
        let roots = self.roots.read().await;
        let path = validate_write_path(&params.path, &roots).map_err(invalid_params)?;
        drop(roots);

        let before_snap = self.snapshot_before(&path, "before re_replace").await;

        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|err| internal_error(format!("failed to read '{}': {err}", params.path)))?;

        if content.len() > WRITE_MAX_BYTES {
            return tool_error(format!(
                "File too large for editing ({}, max {})",
                format_size(content.len()),
                format_size(WRITE_MAX_BYTES)
            ));
        }

        let regex = Regex::new(&params.pattern).map_err(|err| {
            ErrorData::invalid_params(format!("invalid regex pattern: {err}"), None)
        })?;
        let mut count = 0usize;
        for result in regex.find_iter(&content) {
            match result {
                Ok(_) => count += 1,
                Err(err) => return tool_error(format!("regex evaluation error: {err}")),
            }
        }
        if count == 0 {
            return tool_error("pattern did not match anything in the file");
        }

        let replace_all = params.replace_all.unwrap_or(false);
        if !replace_all && count > 1 {
            return tool_error(format!(
                "Found {} matches; set replace_all=true to replace all occurrences",
                count
            ));
        }

        let limit = if replace_all { 0 } else { 1 };
        let new_content = regex
            .try_replacen(&content, limit, params.replacement.as_str())
            .map_err(|err| internal_error(format!("regex replacement error: {err}")))?
            .into_owned();

        if new_content.len() > WRITE_MAX_BYTES {
            return tool_error(format!(
                "Replacement would produce a file too large ({}, max {})",
                format_size(new_content.len()),
                format_size(WRITE_MAX_BYTES)
            ));
        }

        std::fs::write(&path, &new_content)
            .map_err(|err| internal_error(format!("failed to write '{}': {err}", params.path)))?;

        let diff = self
            .snapshot_after_diff(&path, before_snap, "after re_replace")
            .await;

        Ok(Self::mutation_success(
            format!("Replaced {} match(es) in {}", count, params.path),
            diff,
        ))
    }

    pub(crate) async fn edit_file_impl(
        &self,
        params: EditFileParams,
    ) -> Result<CallToolResult, ErrorData> {
        let roots = self.roots.read().await;
        let path = validate_write_path(&params.path, &roots).map_err(invalid_params)?;
        drop(roots);

        let before_snap = self.snapshot_before(&path, "before edit_file").await;

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

        let diff = self
            .snapshot_after_diff(&path, before_snap, "after edit_file")
            .await;

        Ok(Self::mutation_success(
            format!(
                "Edited {} ({} replacement{})",
                params.path,
                replacements,
                if replacements == 1 { "" } else { "s" }
            ),
            diff,
        ))
    }

    pub(crate) async fn list_directory_impl(
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

    pub(crate) async fn search_files_impl(
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

    pub(crate) async fn find_files_impl(
        &self,
        params: FindFilesParams,
    ) -> Result<CallToolResult, ErrorData> {
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
        // The glob crate doesn't understand Windows verbatim prefixes (`\\?\`)
        // produced by canonicalize(), and expects '/' as the path separator on
        // every platform — normalize both before building the pattern.
        let full_pattern = Self::find_files_pattern(&search_path, &params.pattern);
        let glob_results = glob::glob(&full_pattern).map_err(|err| {
            ErrorData::invalid_params(format!("invalid glob pattern: {err}"), None)
        })?;

        let mut paths = Self::collect_found_paths(&search_path, glob_results, max_results);

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
    pub(crate) async fn rollback_file_impl(
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

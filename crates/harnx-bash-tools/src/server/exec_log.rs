// Auto-split from server.rs / handlers.rs for cohesion. See server/mod.rs.
use super::*;
use harnx_mcp::content::WithAudience;

impl BashServer {
    pub(crate) fn select_log_lines(
        line_matches: Vec<(usize, String)>,
        offset: Option<usize>,
        limit: usize,
        tail: Option<usize>,
    ) -> (Vec<(usize, String)>, Vec<String>) {
        let total_lines = line_matches.len();
        let mut notices = Vec::new();
        let selected = if let Some(tail_n) = tail {
            // When combined with `offset`, first skip to the offset line, then
            // tail the remaining window. `offset` is 1-indexed; offset=1 (or
            // absent) means "no skip".
            let skip = offset.unwrap_or(1).saturating_sub(1).min(total_lines);
            let window_len = total_lines - skip;
            if tail_n < window_len {
                notices.push(format!(
                    "showing last {} of {} matching lines",
                    tail_n, window_len
                ));
            }
            let start = total_lines.saturating_sub(tail_n).max(skip);
            line_matches[start..].to_vec()
        } else {
            let start = offset.unwrap_or(1).saturating_sub(1).min(total_lines);
            let end = (start + limit).min(total_lines);
            line_matches[start..end].to_vec()
        };

        (selected, notices)
    }

    pub(crate) fn collect_matching_log_lines(
        content: &str,
        grep_regex: Option<&Regex>,
    ) -> (Vec<(usize, String)>, Option<String>) {
        let mut regex_error = None;
        let numbered_lines = content
            .lines()
            .enumerate()
            .filter_map(|(idx, line)| {
                let line_number = idx + 1;
                match grep_regex {
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
        (numbered_lines, regex_error)
    }

    // -----------------------------------------------------------------------
    // read_exec_log
    // -----------------------------------------------------------------------

    pub(crate) async fn read_exec_log_impl(
        &self,
        params: ReadExecLogParams,
    ) -> Result<CallToolResult, ErrorData> {
        Self::validate_read_exec_log_params(&params)?;
        let path = self.resolve_log_path(&params.execution_id, &params.stream)?;
        self.ensure_regular_log_file(&path).await?;
        let content = self.read_sanitized_log_content(&path).await?;
        let grep_regex = Self::build_grep_regex(params.grep.as_deref())?;
        let (numbered_lines, regex_error) =
            Self::collect_matching_log_lines(&content, grep_regex.as_ref());
        let mut notices = Self::build_regex_notices(regex_error);

        let total_matching_lines = numbered_lines.len();
        if total_matching_lines == 0 {
            return Ok(Self::build_empty_log_result(&params, &path));
        }

        if let Some(offset) = params.offset {
            if offset > total_matching_lines {
                return tool_error(format!(
                    "offset {} is beyond the {} matching lines in {}",
                    offset,
                    total_matching_lines,
                    path.display()
                ));
            }
        }

        let selection =
            Self::select_read_exec_log_lines(numbered_lines, &params, total_matching_lines)?;
        notices.extend(selection.notices);

        let raw_output = selection
            .lines
            .into_iter()
            .map(|(line_number, line)| format!("{line_number}: {line}"))
            .collect::<Vec<_>>()
            .join("\n");

        let truncate_opts = Self::truncate_opts_from(
            params.head_lines,
            params.tail_lines,
            params.max_output_bytes,
        );
        let truncated_output = truncate_output(&raw_output, &truncate_opts);
        Self::append_truncation_notice(&mut notices, &raw_output, &truncated_output);

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
            ContentBlock::text(output).with_audience(vec![Role::Assistant]),
            ContentBlock::text(summary).with_audience(vec![Role::User]),
        ]))
    }

    pub(crate) fn validate_read_exec_log_params(
        params: &ReadExecLogParams,
    ) -> Result<(), ErrorData> {
        if params.stream != "stdout" && params.stream != "stderr" {
            return Err(ErrorData::invalid_params(
                format!(
                    "stream must be 'stdout' or 'stderr', got '{}'",
                    params.stream
                ),
                None,
            ));
        }
        Ok(())
    }

    pub(crate) async fn ensure_regular_log_file(&self, path: &Path) -> Result<(), ErrorData> {
        let metadata = tokio::fs::metadata(path)
            .await
            .map_err(|err| internal_error(format!("cannot access '{}': {err}", path.display())))?;
        if !metadata.is_file() {
            return Err(internal_error(format!(
                "'{}' is not a regular log file.",
                path.display()
            )));
        }
        Ok(())
    }

    pub(crate) async fn read_sanitized_log_content(
        &self,
        path: &Path,
    ) -> Result<String, ErrorData> {
        let raw_content = tokio::fs::read_to_string(path)
            .await
            .map_err(|err| internal_error(format!("failed to read '{}': {err}", path.display())))?;
        Ok(sanitize_output_text(&raw_content))
    }

    pub(crate) fn build_grep_regex(pattern: Option<&str>) -> Result<Option<Regex>, ErrorData> {
        pattern
            .map(|pattern| {
                Regex::new(pattern).map_err(|err| {
                    ErrorData::invalid_params(format!("invalid grep pattern: {err}"), None)
                })
            })
            .transpose()
    }

    pub(crate) fn build_regex_notices(regex_error: Option<String>) -> Vec<String> {
        regex_error
            .into_iter()
            .map(|err| format!("grep evaluation error: {err}"))
            .collect()
    }

    pub(crate) fn build_empty_log_result(
        params: &ReadExecLogParams,
        path: &Path,
    ) -> CallToolResult {
        let mut output = String::from("<no matching lines>");
        if let Some(pattern) = params.grep.as_deref() {
            let _ = write!(output, "\n\n[no lines matched grep pattern '{}']", pattern);
        }
        let summary = format!("Read {} (0 lines)", path.display());
        CallToolResult::success(vec![
            ContentBlock::text(output).with_audience(vec![Role::Assistant]),
            ContentBlock::text(summary).with_audience(vec![Role::User]),
        ])
    }

    pub(crate) fn select_read_exec_log_lines(
        numbered_lines: Vec<(usize, String)>,
        params: &ReadExecLogParams,
        total_matching_lines: usize,
    ) -> Result<ReadExecLogSelection, ErrorData> {
        if let Some(offset) = params.offset {
            if offset == 0 {
                return Err(ErrorData::invalid_params("offset must be >= 1", None));
            }
        }

        let limit = params.limit.unwrap_or(200).max(1);
        let (lines, mut notices) =
            Self::select_log_lines(numbered_lines, params.offset, limit, params.tail);
        // The offset-based "more lines" pagination notice only applies to
        // forward reads. When `tail` is set the selection is anchored to the
        // end of the window, so there are never more lines after it.
        if let (Some(offset), None) = (params.offset, params.tail) {
            let shown_count = lines.len();
            let end = (offset - 1 + shown_count).min(total_matching_lines);
            if end < total_matching_lines {
                notices.push(format!(
                    "{} more matching lines. Use offset={} to continue",
                    total_matching_lines - end,
                    end + 1
                ));
            }
        }

        Ok(ReadExecLogSelection { lines, notices })
    }

    pub(crate) fn resolve_log_path(
        &self,
        execution_id: &str,
        stream: &str,
    ) -> Result<PathBuf, ErrorData> {
        let abs = self
            .inner
            .log_dir
            .join(execution_id)
            .join(format!("{stream}.log"));
        let mut log_allowlist = ResolvedAllowlist::new();
        log_allowlist.insert_read(&self.inner.log_dir);
        validate_path(abs.to_string_lossy().as_ref(), &log_allowlist).map_err(|err| {
            if err.starts_with("Cannot resolve path") {
                ErrorData::invalid_params(
                    format!("cannot resolve execution_id '{execution_id}': {err}"),
                    None,
                )
            } else {
                ErrorData::invalid_params(
                    format!(
                        "execution_id '{}' is outside the bash server temp log directory",
                        execution_id
                    ),
                    None,
                )
            }
        })
    }

    pub(crate) fn truncate_opts_from(
        head_lines: Option<usize>,
        tail_lines: Option<usize>,
        max_output_bytes: Option<usize>,
    ) -> TruncateOpts {
        let default_opts = TruncateOpts::default();
        TruncateOpts {
            head_lines: head_lines.unwrap_or(default_opts.head_lines),
            tail_lines: tail_lines.unwrap_or(default_opts.tail_lines),
            line_head_bytes: default_opts.line_head_bytes,
            line_tail_bytes: default_opts.line_tail_bytes,
            max_output_bytes: max_output_bytes.unwrap_or(default_opts.max_output_bytes),
            ..default_opts
        }
    }

    pub(crate) fn append_truncation_notice(
        notices: &mut Vec<String>,
        raw_output: &str,
        truncated_output: &str,
    ) {
        if truncated_output != raw_output {
            notices.push(format!(
                "output truncated from {} to {}. Use head_lines, tail_lines, or max_output_bytes to see more",
                format_size(raw_output.len()),
                format_size(truncated_output.len())
            ));
        }
    }
}

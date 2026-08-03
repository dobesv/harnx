// Auto-split from server.rs / handlers.rs for cohesion. See server/mod.rs.
use super::*;
use harnx_mcp::content::WithAudience;

impl BashServer {
    // exec
    // -----------------------------------------------------------------------

    pub(crate) async fn exec_command_impl(
        &self,
        params: ExecCommandParams,
    ) -> Result<CallToolResult, ErrorData> {
        let prepared = self
            .prepare_exec(
                &params.command,
                params.working_dir.as_deref(),
                params.env.as_ref(),
                "before exec",
            )
            .await?;
        let ExecPreparation {
            working_dir,
            snapshot_decision,
            before_snap_ids: before_snaps,
        } = prepared;

        let timeout_secs = params.timeout_secs.unwrap_or(120);
        let truncate_opts = Self::truncate_opts_from(
            params.head_lines,
            params.tail_lines,
            params.max_output_bytes,
        );

        let ExecLog {
            exec_dir,
            stdout_log_path,
            stderr_log_path,
            execution_id,
            stdout_file,
            stderr_file,
        } = self.setup_exec_log().await?;

        let command = self
            .build_command(
                CommandBuildCtx {
                    command: &params.command,
                    working_dir: &working_dir,
                    exec_dir: &exec_dir,
                    env: params.env.as_ref(),
                },
                Stdio::piped(),
                Stdio::piped(),
            )
            .await?;
        let RunOutcome {
            status,
            timed_out,
            stdout_str,
            stderr_str,
        } = Self::run_to_completion(
            command,
            timeout_secs,
            LogTargets {
                stdout_file,
                stderr_file,
                stdout_log_path: &stdout_log_path,
                stderr_log_path: &stderr_log_path,
            },
        )
        .await?;

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
                self.build_exec_success_result(ExitResultCtx {
                    execution_id: &execution_id,
                    command: &params.command,
                    working_dir: &working_dir,
                    stdout_log_path: &stdout_log_path,
                    stderr_log_path: &stderr_log_path,
                    total_lines,
                    total_bytes,
                    exit_code: status.code().unwrap_or(-1),
                    streams_block,
                    before_snaps: &before_snaps,
                    snapshot_decision: &snapshot_decision,
                })
                .await
            }
            (Some(status), true) => {
                let _ = status;
                self.build_timeout_result(TimeoutResultCtx {
                    command: &params.command,
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
                })
            }
            (None, _) => tool_error("process exited without status".to_string()),
        }
    }

    // -----------------------------------------------------------------------
    // rollback_file
    // -----------------------------------------------------------------------

    pub(crate) async fn rollback_file_impl(
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

        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "Rolled back to harnx snapshot {}; new commit {} created (can be reverted)",
            &params.commit_id[..8.min(params.commit_id.len())],
            new_commit_id.to_hex(),
        ))]))
    }

    pub(crate) async fn prepare_exec(
        &self,
        command: &str,
        working_dir: Option<&str>,
        extra_env: Option<&HashMap<String, String>>,
        snapshot_label: &str,
    ) -> Result<ExecPreparation, ErrorData> {
        if let Some(extra_env) = extra_env {
            Self::validate_extra_env(extra_env)?;
        }
        if command.trim().is_empty() {
            return Err(ErrorData::invalid_params("command cannot be empty", None));
        }

        let working_dir = self.resolve_working_dir(working_dir).await?;
        let snapshot_decision = Self::snapshot_decision_for_command(command, &working_dir);
        let before_snap_ids = self
            .take_snapshots(&snapshot_decision, &working_dir, snapshot_label)
            .await;

        Ok(ExecPreparation {
            working_dir,
            snapshot_decision,
            before_snap_ids,
        })
    }

    pub(crate) fn snapshot_decision_for_command(
        command: &str,
        working_dir: &Path,
    ) -> SnapshotDecision {
        classify_command(command, working_dir)
    }

    pub(crate) async fn take_snapshots(
        &self,
        decision: &SnapshotDecision,
        working_dir: &Path,
        label: &str,
    ) -> Vec<(PathBuf, gix::ObjectId)> {
        let phase = label.split_whitespace().next().unwrap_or("snapshot");
        match decision {
            SnapshotDecision::ReadOnly => vec![],
            SnapshotDecision::Targeted(paths) => self
                .inner
                .history
                .snapshot_repos_for_dir_targeted(working_dir, paths, label)
                .await
                .unwrap_or_else(|e| {
                    log::warn!("history {phase}-snapshot failed: {e}");
                    vec![]
                }),
            SnapshotDecision::FullSnapshot => self
                .inner
                .history
                .snapshot_repos_for_dir(working_dir, label)
                .await
                .unwrap_or_else(|e| {
                    log::warn!("history {phase}-snapshot failed: {e}");
                    vec![]
                }),
        }
    }

    pub(crate) async fn diff_snapshots(
        &self,
        before: &[(PathBuf, gix::ObjectId)],
        decision: &SnapshotDecision,
        working_dir: &Path,
        label: &str,
    ) -> Vec<String> {
        if before.is_empty() {
            return vec![];
        }

        let after = self.take_snapshots(decision, working_dir, label).await;
        let mut diff_parts = Vec::new();
        for (repo_dir, before_id) in before {
            if let Some((_, after_id)) = after.iter().find(|(dir, _)| dir == repo_dir) {
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
        diff_parts
    }

    pub(crate) async fn build_exec_success_result(
        &self,
        ctx: ExitResultCtx<'_>,
    ) -> Result<CallToolResult, ErrorData> {
        let summary = format!(
            "exit_code: {}, {} lines, {}",
            ctx.exit_code,
            ctx.total_lines,
            format_size(ctx.total_bytes)
        );
        self.build_exit_result(ctx, summary, "after exec").await
    }

    /// Shared assembly for "exited" results: metadata header + streams block,
    /// then after-snapshot diff. `summary` and `snapshot_label` differ per tool.
    pub(crate) async fn build_exit_result(
        &self,
        ctx: ExitResultCtx<'_>,
        summary: String,
        snapshot_label: &str,
    ) -> Result<CallToolResult, ErrorData> {
        let mut output = String::new();
        render_metadata_header(
            &mut output,
            MetadataHeader {
                execution_id: Some(ctx.execution_id),
                status: Some("exited"),
                exit_code: Some(ctx.exit_code),
                command: Some(ctx.command),
                working_dir: Some(ctx.working_dir),
                stdout_log_path: Some(ctx.stdout_log_path),
                stderr_log_path: Some(ctx.stderr_log_path),
                total_lines: Some(ctx.total_lines),
                total_bytes: Some(ctx.total_bytes),
            },
        );
        let _ = write!(output, "\n{}", ctx.streams_block);
        let diff_parts = self
            .diff_snapshots(
                ctx.before_snaps,
                ctx.snapshot_decision,
                ctx.working_dir,
                snapshot_label,
            )
            .await;
        Ok(Self::build_success_result(output, summary, diff_parts))
    }

    pub(crate) fn build_timeout_result(
        &self,
        ctx: TimeoutResultCtx<'_>,
    ) -> Result<CallToolResult, ErrorData> {
        tool_error(render_timeout_message(TimeoutRenderContext {
            command: ctx.command,
            working_dir: ctx.working_dir,
            execution_id: ctx.execution_id,
            timeout_secs: ctx.timeout_secs,
            total_lines: ctx.total_lines,
            total_bytes: ctx.total_bytes,
            stdout: ctx.stdout,
            stderr: ctx.stderr,
            truncate_opts: ctx.truncate_opts,
            stdout_log_path: ctx.stdout_log_path,
            stderr_log_path: ctx.stderr_log_path,
        }))
    }

    pub(crate) fn build_success_result(
        mut output: String,
        summary: String,
        diff_parts: Vec<String>,
    ) -> CallToolResult {
        if !output.ends_with('\n') {
            output.push('\n');
        }
        let mut contents = vec![
            ContentBlock::text(output).with_audience(vec![Role::Assistant]),
            ContentBlock::text(summary).with_audience(vec![Role::User]),
        ];
        for diff in diff_parts {
            contents.push(ContentBlock::text(diff));
        }
        CallToolResult::success(contents)
    }
}

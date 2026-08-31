// Auto-split from server.rs / handlers.rs for cohesion. See server/mod.rs.
use super::*;
use harnx_toolset_server::content::WithAudience;

impl BashServer {
    // exec
    // -----------------------------------------------------------------------

    pub(crate) async fn exec_command_impl(
        &self,
        params: ExecCommandParams,
    ) -> Result<CallToolResult, ErrorData> {
        let timeout_secs = resolve_command_timeout(params.timeout_secs);
        self.exec_pipeline(ExecPipelineParams {
            command: &params.command,
            working_dir: params.working_dir.as_deref(),
            extra_env: params.env.as_ref(),
            timeout_secs,
            truncate_opts: Self::truncate_opts_from(
                params.head_lines,
                params.tail_lines,
                params.max_output_bytes,
            ),
            template_sandbox: None,
        })
        .await
    }

    pub(crate) async fn invoke_template(
        &self,
        name: &str,
        args: &Map<String, Value>,
    ) -> Result<CallToolResult, ErrorData> {
        let registered = self.inner.templates.get(name).ok_or_else(|| {
            ErrorData::invalid_params(format!("unknown tool template: {name}"), None)
        })?;
        let mut script_args = args.clone();
        let requested_timeout = parse_template_timeout(script_args.remove(COMMAND_TIMEOUT_ARG))?;
        let bound = registered
            .template
            .validate_and_bind(&script_args)
            .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?;
        let command = registered
            .template
            .render_script(&bound)
            .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?;

        // Parameter bindings take precedence over author-supplied top-level env.
        let mut extra_env = registered.template.env.clone().unwrap_or_default();
        extra_env.extend(bound);
        if !registered.sandbox_enabled {
            log::warn!("tool '{name}' runs UNSANDBOXED (sandbox.enabled=false)");
            if registered.ignored_grants {
                log::warn!("tool '{name}' ignores sandbox grants because sandbox.enabled=false");
            }
        }

        self.exec_pipeline(ExecPipelineParams {
            command: &command,
            working_dir: None,
            extra_env: Some(&extra_env),
            timeout_secs: resolve_command_timeout(requested_timeout),
            truncate_opts: Self::truncate_opts_from(None, None, None),
            template_sandbox: Some(TemplateSandbox {
                enabled: registered.sandbox_enabled,
                read_paths: &registered.read_paths,
                write_paths: &registered.write_paths,
                pass_env: &registered.pass_env,
                no_network: registered.no_network,
            }),
        })
        .await
    }

    pub(crate) async fn exec_pipeline(
        &self,
        params: ExecPipelineParams<'_>,
    ) -> Result<CallToolResult, ErrorData> {
        let prepared = self
            .prepare_exec(
                params.command,
                params.working_dir,
                params.extra_env,
                "before exec",
            )
            .await?;
        let ExecPreparation {
            working_dir,
            snapshot_decision,
            before_snap_ids: before_snaps,
        } = prepared;

        let ExecLog {
            exec_dir,
            stdout_log_path,
            stderr_log_path,
            execution_id,
            stdout_file,
            stderr_file,
        } = self.setup_exec_log().await?;

        let command = self
            .build_command_with_sandbox(
                CommandBuildCtx {
                    command: params.command,
                    working_dir: &working_dir,
                    exec_dir: &exec_dir,
                    env: params.extra_env,
                },
                params.template_sandbox,
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
            params.timeout_secs,
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
                &params.truncate_opts,
                None,
                &execution_id,
                &stdout_log_path,
                &stderr_log_path,
            );
        let total_lines = stdout_lines + stderr_lines;
        let total_bytes = stdout_bytes_len + stderr_bytes_len;

        if timed_out {
            return self.build_timeout_result(TimeoutResultCtx {
                command: params.command,
                working_dir: &working_dir,
                execution_id: &execution_id,
                timeout_secs: params
                    .timeout_secs
                    .expect("timed-out commands always have a deadline"),
                total_lines,
                total_bytes,
                stdout: &stdout_str,
                stderr: &stderr_str,
                truncate_opts: &params.truncate_opts,
                stdout_log_path: &stdout_log_path,
                stderr_log_path: &stderr_log_path,
            });
        }

        self.build_exec_success_result(ExitResultCtx {
            execution_id: &execution_id,
            command: params.command,
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

    // -----------------------------------------------------------------------
    // rollback_file
    // -----------------------------------------------------------------------

    pub(crate) async fn rollback_file_impl(
        &self,
        params: RollbackParams,
    ) -> Result<CallToolResult, ErrorData> {
        let path = validate_write_path(&params.repo_path, &self.inner.allowlist)
            .map_err(invalid_params)?;

        let commit_id = ObjectId::from_hex(params.commit_id.as_bytes())
            .map_err(|e| ErrorData::invalid_params(format!("invalid commit_id: {e}"), None))?;

        let repo_dir = harnx_mcp_history::discover::find_repo_for_path(&path).ok_or_else(|| {
            ErrorData::invalid_params("path is not inside a git repository".to_string(), None)
        })?;
        validate_write_path(&repo_dir.to_string_lossy(), &self.inner.allowlist)
            .map_err(invalid_params)?;

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

fn resolve_command_timeout(requested: Option<u64>) -> Option<u64> {
    match requested {
        None => Some(DEFAULT_COMMAND_TIMEOUT_SECS),
        Some(0) => None,
        Some(seconds) => Some(seconds),
    }
}

fn parse_template_timeout(value: Option<Value>) -> Result<Option<u64>, ErrorData> {
    match value {
        None => Ok(None),
        Some(Value::Number(number)) => number.as_u64().map(Some).ok_or_else(|| {
            ErrorData::invalid_params("timeout_secs must be a non-negative integer", None)
        }),
        Some(_) => Err(ErrorData::invalid_params(
            "timeout_secs must be a non-negative integer",
            None,
        )),
    }
}

#[cfg(test)]
mod timeout_tests {
    use super::{parse_template_timeout, resolve_command_timeout};
    use crate::server::DEFAULT_COMMAND_TIMEOUT_SECS;
    use serde_json::json;

    #[test]
    fn resolves_default_override_and_unlimited_timeouts() {
        assert_eq!(
            resolve_command_timeout(None),
            Some(DEFAULT_COMMAND_TIMEOUT_SECS)
        );
        assert_eq!(resolve_command_timeout(Some(17)), Some(17));
        assert_eq!(resolve_command_timeout(Some(0)), None);
    }

    #[test]
    fn template_timeout_accepts_non_negative_integers_only() {
        assert_eq!(parse_template_timeout(None).unwrap(), None);
        assert_eq!(parse_template_timeout(Some(json!(0))).unwrap(), Some(0));
        assert_eq!(parse_template_timeout(Some(json!(17))).unwrap(), Some(17));
        assert!(parse_template_timeout(Some(json!(-1))).is_err());
        assert!(parse_template_timeout(Some(json!(1.5))).is_err());
        assert!(parse_template_timeout(Some(json!("17"))).is_err());
    }
}

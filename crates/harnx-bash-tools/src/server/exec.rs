// Auto-split from server.rs / handlers.rs for cohesion. See server/mod.rs.
use super::*;
use crate::tool_template::protocol::TOOL_PROTOCOL_ENV;
use harnx_toolset_server::content::WithAudience;

impl BashServer {
    // exec
    // -----------------------------------------------------------------------

    pub(crate) async fn exec_command_impl(
        &self,
        params: ExecCommandParams,
    ) -> Result<CallToolResult, ErrorData> {
        let timeout_secs = Some(resolve_command_timeout(params.timeout_secs)?);
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
            include_log_hints: true,
            line_cap_notes: harnx_toolset::line_cap_notes(params.head_lines, params.tail_lines),
            template_sandbox: None,
            on_user_message: OnUserMessage::Ignore,
            protocol: None,
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
        let timeout_secs = resolve_command_timeout(requested_timeout)?;
        let bound = registered
            .template
            .validate_and_bind(&script_args)
            .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?;
        let command = registered
            .template
            .render_script(&bound.env)
            .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?;

        // Parameter bindings take precedence over author-supplied top-level env.
        let mut extra_env = registered.template.env.clone().unwrap_or_default();
        extra_env.extend(bound.env);
        // Inserted last so a template cannot override the runtime-declared protocol.
        if let Some(protocol) = registered.protocol {
            extra_env.insert(
                TOOL_PROTOCOL_ENV.to_string(),
                protocol.env_value().to_string(),
            );
        }
        if !registered.sandbox_enabled {
            log::warn!("tool '{name}' runs UNSANDBOXED (sandbox.enabled=false)");
            if registered.ignored_grants {
                log::warn!("tool '{name}' ignores sandbox grants because sandbox.enabled=false");
            }
        }

        self.exec_pipeline(ExecPipelineParams {
            command: &command,
            on_user_message: registered.on_user_message,
            protocol: registered.protocol,
            working_dir: None,
            extra_env: Some(&extra_env),
            timeout_secs: Some(timeout_secs),
            truncate_opts: Self::truncate_opts_from(
                bound.output.head_lines,
                bound.output.tail_lines,
                bound.output.max_output_bytes,
            ),
            include_log_hints: false,
            line_cap_notes: harnx_toolset::line_cap_notes(
                bound.output.head_lines,
                bound.output.tail_lines,
            ),
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

        let outcome = self
            .run_exec_command(
                &params,
                CommandBuildCtx {
                    command: params.command,
                    working_dir: &working_dir,
                    exec_dir: &exec_dir,
                    env: params.extra_env,
                },
                LogTargets {
                    stdout_file,
                    stderr_file,
                    stdout_log_path: &stdout_log_path,
                    stderr_log_path: &stderr_log_path,
                },
            )
            .await?;

        let finish = ExecFinish {
            execution_id: &execution_id,
            working_dir: &working_dir,
            stdout_log_path: &stdout_log_path,
            stderr_log_path: &stderr_log_path,
            before_snaps: &before_snaps,
            snapshot_decision: &snapshot_decision,
        };
        if params.protocol.is_none() && crate::toolset::invocation_wants_structured_result() {
            return Ok(self.script_exec_result(&params, outcome, finish).await);
        }
        self.model_exec_result(&params, outcome, finish).await
    }

    /// The result a `bash` script reads: separate stdout and stderr and the
    /// exit status, with the same one-line summary a model-style result has.
    /// A timeout is a normal result here so the partial output reaches the pipe.
    async fn script_exec_result(
        &self,
        params: &ExecPipelineParams<'_>,
        outcome: RunOutcome,
        finish: ExecFinish<'_>,
    ) -> CallToolResult {
        // The diff is taken even after a timeout: the command may have
        // changed files before it was stopped.
        let diffs = self
            .diff_snapshots(
                finish.before_snaps,
                finish.snapshot_decision,
                finish.working_dir,
                "after exec",
            )
            .await;
        let mut ended = script_result::ended(outcome.kind, outcome.status);
        ended.notes.extend(params.line_cap_notes.iter().cloned());
        let lines = outcome.stdout_str.lines().count() + outcome.stderr_str.lines().count();
        let bytes = outcome.stdout_str.len() + outcome.stderr_str.len();
        let prefix = match outcome.kind {
            ExitKind::Interrupted => "stopped_by: new_message".to_owned(),
            ExitKind::TimedOut { .. } | ExitKind::Exited => {
                format!("exit_code: {}", ended.exit_code)
            }
        };
        script_result::command_result(
            script_result::CommandReport {
                execution_id: finish.execution_id,
                status: ended.status,
                exit_code: ended.exit_code,
                stdout: &outcome.stdout_str,
                stderr: &outcome.stderr_str,
                stdout_log_path: &finish.stdout_log_path.to_string_lossy(),
                stderr_log_path: &finish.stderr_log_path.to_string_lossy(),
                max_output_bytes: params.truncate_opts.max_output_bytes,
                log_command: params
                    .include_log_hints
                    .then(script_result::read_log_command),
                log_unfiltered: true,
                notes: ended.notes,
                diffs,
            },
            format!("{prefix}, {lines} lines, {}", format_size(bytes)),
        )
    }

    // qual:allow(iosp) — a timeout answers with an error built from the rendered output and an exit with the success report, so the function branches between two builders.
    async fn model_exec_result(
        &self,
        params: &ExecPipelineParams<'_>,
        outcome: RunOutcome,
        finish: ExecFinish<'_>,
    ) -> Result<CallToolResult, ErrorData> {
        let (streams_block, total_lines, total_bytes) = render_run_output(
            params,
            &outcome,
            StreamLogHints {
                execution_id: finish.execution_id,
                stdout_log_path: finish.stdout_log_path,
                stderr_log_path: finish.stderr_log_path,
            },
        );
        let RunOutcome { status, kind, .. } = outcome;

        if let ExitKind::TimedOut { deadline_secs } = kind {
            return self.build_timeout_result(TimeoutResultCtx {
                command: params.command,
                working_dir: finish.working_dir,
                execution_id: finish.execution_id,
                timeout_secs: deadline_secs,
                total_lines,
                total_bytes,
                streams_block: &streams_block,
                stdout_log_path: finish.stdout_log_path,
                stderr_log_path: finish.stderr_log_path,
            });
        }

        self.build_exec_success_result(
            ExitResultCtx {
                execution_id: finish.execution_id,
                command: params.command,
                working_dir: finish.working_dir,
                stdout_log_path: finish.stdout_log_path,
                stderr_log_path: finish.stderr_log_path,
                total_lines,
                total_bytes,
                exit_code: status.code().unwrap_or(-1),
                streams_block,
                before_snaps: finish.before_snaps,
                snapshot_decision: finish.snapshot_decision,
            },
            kind,
        )
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

        let repo_dir = harnx_git_history::discover::find_repo_for_path(&path).ok_or_else(|| {
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
        kind: ExitKind,
    ) -> Result<CallToolResult, ErrorData> {
        let prefix = match kind {
            ExitKind::Interrupted => "stopped_by: new_message".to_owned(),
            ExitKind::TimedOut { .. } | ExitKind::Exited => {
                format!("exit_code: {}", ctx.exit_code)
            }
        };
        let summary = format!(
            "{prefix}, {} lines, {}",
            ctx.total_lines,
            format_size(ctx.total_bytes)
        );
        let diff_parts = self
            .diff_snapshots(
                ctx.before_snaps,
                ctx.snapshot_decision,
                ctx.working_dir,
                "after exec",
            )
            .await;
        Ok(Self::exit_result_with_diff(ctx, kind, summary, diff_parts))
    }

    /// Shared assembly for "exited" results (exec and wait): metadata header
    /// and streams block, then the after-snapshot diff each tool took.
    pub(crate) fn exit_result_with_diff(
        ctx: ExitResultCtx<'_>,
        kind: ExitKind,
        summary: String,
        diff_parts: Vec<String>,
    ) -> CallToolResult {
        let mut output = String::new();
        render_metadata_header(
            &mut output,
            MetadataHeader {
                execution_id: Some(ctx.execution_id),
                status: Some(kind.status()),
                stopped_by: (kind == ExitKind::Interrupted).then_some("new_message"),
                exit_code: (kind == ExitKind::Exited).then_some(ctx.exit_code),
                command: Some(ctx.command),
                working_dir: Some(ctx.working_dir),
                stdout_log_path: Some(ctx.stdout_log_path),
                stderr_log_path: Some(ctx.stderr_log_path),
                total_lines: Some(ctx.total_lines),
                total_bytes: Some(ctx.total_bytes),
                running_for: None,
            },
        );
        let _ = write!(output, "\n{}", ctx.streams_block);
        Self::build_success_result(output, summary, diff_parts)
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
            streams_block: ctx.streams_block,
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

/// What a finished foreground run needs to build its result.
struct ExecFinish<'a> {
    execution_id: &'a str,
    working_dir: &'a Path,
    stdout_log_path: &'a Path,
    stderr_log_path: &'a Path,
    before_snaps: &'a [(PathBuf, gix::ObjectId)],
    snapshot_decision: &'a SnapshotDecision,
}

/// The result's output block with its line and byte totals. A protocol
/// template's stdout carries progress messages, so only its stderr is shown
/// and counted.
fn render_run_output(
    params: &ExecPipelineParams<'_>,
    outcome: &RunOutcome,
    log_hints: StreamLogHints<'_>,
) -> (String, usize, usize) {
    if params.protocol.is_some() {
        return render_stderr_block(&outcome.stderr_str, &params.truncate_opts);
    }
    // exec does not expose a grep param; pass None
    let (block, stdout_lines, stderr_lines, stdout_bytes, stderr_bytes) = render_streams_block(
        &outcome.stdout_str,
        &outcome.stderr_str,
        &params.truncate_opts,
        None,
        params.include_log_hints.then_some(log_hints),
    );
    (
        block,
        stdout_lines + stderr_lines,
        stdout_bytes + stderr_bytes,
    )
}

/// The command deadline in seconds: the default when unset, otherwise the
/// requested value within `1..=COMMAND_TIMEOUT_MAX_SECS`.
pub(super) fn resolve_command_timeout(requested: Option<u64>) -> Result<u64, ErrorData> {
    match requested {
        None => Ok(COMMAND_TIMEOUT_MAX_SECS),
        Some(seconds) if (1..=COMMAND_TIMEOUT_MAX_SECS).contains(&seconds) => Ok(seconds),
        Some(_) => Err(ErrorData::invalid_params(
            format!("timeout_secs must be between 1 and {COMMAND_TIMEOUT_MAX_SECS}"),
            None,
        )),
    }
}

fn parse_template_timeout(value: Option<Value>) -> Result<Option<u64>, ErrorData> {
    match value {
        None | Some(Value::Null) => Ok(None),
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
    use crate::server::COMMAND_TIMEOUT_MAX_SECS;
    use serde_json::json;

    #[test]
    fn resolves_default_override_and_rejects_out_of_range() {
        assert_eq!(
            resolve_command_timeout(None).unwrap(),
            COMMAND_TIMEOUT_MAX_SECS
        );
        assert_eq!(resolve_command_timeout(Some(17)).unwrap(), 17);
        for bad in [0, COMMAND_TIMEOUT_MAX_SECS + 1] {
            let error = resolve_command_timeout(Some(bad)).unwrap_err();
            assert!(error.message.contains("between 1 and 86400"), "{error:?}");
        }
    }

    #[test]
    fn template_timeout_accepts_non_negative_integers_only() {
        assert_eq!(parse_template_timeout(None).unwrap(), None);
        assert_eq!(parse_template_timeout(Some(json!(null))).unwrap(), None);
        assert_eq!(parse_template_timeout(Some(json!(0))).unwrap(), Some(0));
        assert_eq!(parse_template_timeout(Some(json!(17))).unwrap(), Some(17));
        assert!(parse_template_timeout(Some(json!(-1))).is_err());
        assert!(parse_template_timeout(Some(json!(1.5))).is_err());
        assert!(parse_template_timeout(Some(json!("17"))).is_err());
    }
}

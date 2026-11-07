// Auto-split from server.rs / handlers.rs for cohesion. See server/mod.rs.
use super::*;

struct WaitOutput {
    streams_block: String,
    total_lines: usize,
    total_bytes: usize,
}

fn compile_wait_grep(options: &WaitOptions) -> Result<Option<Regex>, ErrorData> {
    options
        .grep
        .as_deref()
        .map(|pattern| {
            Regex::new(pattern).map_err(|err| {
                ErrorData::invalid_params(format!("invalid grep pattern: {err}"), None)
            })
        })
        .transpose()
}

impl BashServer {
    // -----------------------------------------------------------------------
    // spawn
    // -----------------------------------------------------------------------

    pub(crate) async fn spawn_impl(
        &self,
        params: SpawnCommandParams,
    ) -> Result<CallToolResult, ErrorData> {
        // Validate the nested wait's grep before prepare_exec so an invalid
        // pattern fails the call with zero side effects: no logs, snapshots,
        // or spawned process to clean up.
        let grep_regex = params
            .wait
            .as_ref()
            .map(compile_wait_grep)
            .transpose()?
            .flatten();
        let prepared = self
            .prepare_exec(
                &params.command,
                params.working_dir.as_deref(),
                params.env.as_ref(),
                "before spawn",
            )
            .await?;
        let ExecPreparation {
            working_dir,
            snapshot_decision,
            before_snap_ids,
        } = prepared;
        self.ensure_log_dir().await?;

        let exec_dir = self.next_exec_dir()?.keep();
        let stdout_log_path = exec_dir.join("stdout.log");
        let stderr_log_path = exec_dir.join("stderr.log");
        let execution_id = exec_dir.file_name().unwrap().to_string_lossy().into_owned();

        let child = self
            .spawn_background_child(
                CommandBuildCtx {
                    command: &params.command,
                    working_dir: &working_dir,
                    exec_dir: &exec_dir,
                    env: params.env.as_ref(),
                },
                &stdout_log_path,
                &stderr_log_path,
            )
            .await?;

        let entry = SpawnedProcess {
            child,
            command: params.command.clone(),
            working_dir: working_dir.clone(),
            stdout_log_path: stdout_log_path.clone(),
            stderr_log_path: stderr_log_path.clone(),
            before_snap_ids,
            snapshot_decision: snapshot_decision.clone(),
            started_at: std::time::Instant::now(),
        };

        self.store_spawned_process(execution_id.clone(), entry)
            .await;

        if let Some(options) = params.wait {
            let wait_params = WaitParams {
                execution_id,
                options,
            };
            return self.wait_for_process(wait_params, grep_regex).await;
        }

        Ok(Self::build_spawn_result(SpawnResultCtx {
            execution_id: &execution_id,
            command: &params.command,
            working_dir: &working_dir,
            stdout_log_path: &stdout_log_path,
            stderr_log_path: &stderr_log_path,
        }))
    }

    async fn spawn_background_child(
        &self,
        ctx: CommandBuildCtx<'_>,
        stdout_log_path: &Path,
        stderr_log_path: &Path,
    ) -> Result<Box<dyn ChildWrapper>, ErrorData> {
        let stdout_file = create_log_file(stdout_log_path, "stdout")?;
        let stderr_file = create_log_file(stderr_log_path, "stderr")?;

        let mut command = self
            .build_command(ctx, Stdio::from(stdout_file), Stdio::from(stderr_file))
            .await?;
        #[cfg(unix)]
        command.wrap(ProcessGroup::leader());
        #[cfg(windows)]
        command.wrap(JobObject);

        command
            .spawn()
            .map_err(|err| internal_error(format!("failed to spawn command: {err}")))
    }

    // -----------------------------------------------------------------------
    // wait
    // -----------------------------------------------------------------------

    pub(crate) async fn wait_impl(&self, params: WaitParams) -> Result<CallToolResult, ErrorData> {
        let grep_regex = compile_wait_grep(&params.options)?;
        self.wait_for_process(params, grep_regex).await
    }

    async fn wait_for_process(
        &self,
        params: WaitParams,
        grep_regex: Option<Regex>,
    ) -> Result<CallToolResult, ErrorData> {
        let timeout_secs = params.options.effective_timeout_secs();
        // The entry is removed up front so wait/terminate stay serialized per
        // execution_id; every non-exit path below must re-store it or the
        // process becomes untracked. A caller cancellation dropped mid-await
        // loses the handle entirely — for spawn's initial wait the
        // execution_id was never returned, so the running command is
        // unreachable (known limitation, docs/bash-mcp-server.md).
        let mut entry = {
            let mut map = self.inner.spawned.lock().await;
            map.remove(&params.execution_id).ok_or_else(|| {
                ErrorData::invalid_params(
                    format!(
                        "execution_id '{}' is not a tracked background process (or already waited on)",
                        params.execution_id
                    ),
                    None,
                )
            })?
        };

        let timeout = Duration::from_secs(timeout_secs);
        let status = match tokio::time::timeout(timeout, entry.child.wait()).await {
            Ok(Ok(status)) => Some(status),
            Ok(Err(err)) => {
                // A failed wait does not prove the process exited. Keep its handle manageable.
                self.store_spawned_process(params.execution_id.clone(), entry)
                    .await;
                return Err(internal_error(format!(
                    "failed waiting for execution_id '{}': {err}",
                    params.execution_id
                )));
            }
            Err(_) => None,
        };

        let output = Self::read_wait_output(
            &params.execution_id,
            &entry,
            &params.options,
            grep_regex.as_ref(),
        )
        .await;

        if let Some(status) = status {
            return self
                .build_wait_exit_result(ExitResultCtx {
                    execution_id: &params.execution_id,
                    command: &entry.command,
                    working_dir: &entry.working_dir,
                    stdout_log_path: &entry.stdout_log_path,
                    stderr_log_path: &entry.stderr_log_path,
                    total_lines: output.total_lines,
                    total_bytes: output.total_bytes,
                    exit_code: status.code().unwrap_or(-1),
                    streams_block: output.streams_block,
                    before_snaps: &entry.before_snap_ids,
                    snapshot_decision: &entry.snapshot_decision,
                })
                .await;
        }

        let result =
            Self::build_wait_running_result(&params.execution_id, &entry, timeout_secs, output);
        self.store_spawned_process(params.execution_id, entry).await;
        Ok(result)
    }

    // Single output-reading path shared by the exit and running results:
    // both outcomes must apply identical grep/filter/truncation semantics
    // (enforced by the running_and_completed_wait_share_* tests).
    async fn read_wait_output(
        execution_id: &str,
        entry: &SpawnedProcess,
        options: &WaitOptions,
        grep_regex: Option<&Regex>,
    ) -> WaitOutput {
        let truncate_opts = Self::truncate_opts_from(
            options.head_lines,
            options.tail_lines,
            options.max_output_bytes,
        );
        let stdout_content = tokio::fs::read_to_string(&entry.stdout_log_path)
            .await
            .unwrap_or_default();
        let stderr_content = tokio::fs::read_to_string(&entry.stderr_log_path)
            .await
            .unwrap_or_default();
        let (streams_block, stdout_lines, stderr_lines, stdout_bytes_len, stderr_bytes_len) =
            render_streams_block(
                &stdout_content,
                &stderr_content,
                &truncate_opts,
                grep_regex,
                execution_id,
                &entry.stdout_log_path,
                &entry.stderr_log_path,
            );
        WaitOutput {
            streams_block,
            total_lines: stdout_lines + stderr_lines,
            total_bytes: stdout_bytes_len + stderr_bytes_len,
        }
    }

    // -----------------------------------------------------------------------
    // terminate
    // -----------------------------------------------------------------------

    // `return` is required here: the cfg(unix) and cfg(windows) blocks coexist
    // textually, so the unix branch is not syntactically the function tail.
    #[allow(clippy::needless_return)]
    pub(crate) async fn terminate_impl(
        &self,
        params: TerminateParams,
    ) -> Result<CallToolResult, ErrorData> {
        let normalized = params.signal.as_deref().unwrap_or("SIGTERM").to_uppercase();

        #[cfg(unix)]
        {
            let signal = Self::parse_signal(&normalized)?;
            let map = self.inner.spawned.lock().await;
            let entry = Self::get_spawned_entry(&map, &params.execution_id)?;
            self.send_signal(entry.child.as_ref(), signal, &params.execution_id)
                .await?;
            return Ok(Self::build_terminate_result(
                &params.execution_id,
                &normalized,
                entry,
            ));
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
            self.send_signal(entry.child.as_mut(), &normalized, &params.execution_id)
                .await?;
            return Ok(Self::build_terminate_result(
                &params.execution_id,
                &normalized,
                entry,
            ));
        }
    }

    #[cfg(unix)]
    pub(crate) fn parse_signal(name: &str) -> Result<(i32, &'static str), ErrorData> {
        match name {
            "SIGTERM" | "TERM" => Ok((libc::SIGTERM, "SIGTERM")),
            "SIGKILL" | "KILL" => Ok((libc::SIGKILL, "SIGKILL")),
            "SIGINT" | "INT" => Ok((libc::SIGINT, "SIGINT")),
            "SIGHUP" | "HUP" => Ok((libc::SIGHUP, "SIGHUP")),
            other => Err(ErrorData::invalid_params(
                format!("unsupported signal: {other}"),
                None,
            )),
        }
    }

    #[cfg(unix)]
    pub(crate) fn get_spawned_entry<'a>(
        map: &'a HashMap<String, SpawnedProcess>,
        execution_id: &str,
    ) -> Result<&'a SpawnedProcess, ErrorData> {
        map.get(execution_id).ok_or_else(|| {
            ErrorData::invalid_params(
                format!(
                    "execution_id '{}' is not a tracked background process",
                    execution_id
                ),
                None,
            )
        })
    }

    #[cfg(unix)]
    pub(crate) async fn send_signal(
        &self,
        child: &dyn ChildWrapper,
        signal: (i32, &'static str),
        execution_id: &str,
    ) -> Result<(), ErrorData> {
        child.signal(signal.0).map_err(|err| {
            internal_error(format!(
                "failed to send {} to execution_id '{}': {err}",
                signal.1, execution_id
            ))
        })
    }

    #[cfg(windows)]
    pub(crate) async fn send_signal(
        &self,
        child: &mut dyn ChildWrapper,
        signal: &str,
        execution_id: &str,
    ) -> Result<(), ErrorData> {
        child.start_kill().map_err(|err| {
            internal_error(format!(
                "failed to terminate execution_id '{}': {err}",
                execution_id
            ))
        })?;
        let _ = signal;
        Ok(())
    }

    pub(crate) fn build_terminate_result(
        execution_id: &str,
        signal: &str,
        entry: &SpawnedProcess,
    ) -> CallToolResult {
        let mut output = String::new();
        render_metadata_header(
            &mut output,
            MetadataHeader {
                execution_id: Some(execution_id),
                status: Some("terminated"),
                exit_code: None,
                command: Some(&entry.command),
                working_dir: Some(&entry.working_dir),
                stdout_log_path: Some(&entry.stdout_log_path),
                stderr_log_path: Some(&entry.stderr_log_path),
                total_lines: None,
                total_bytes: None,
                running_for: None,
            },
        );
        let _ = write!(output, "\n\nsignal: {}", signal);
        let summary = format!("sent {} to {}", signal, execution_id);
        Self::build_success_result(output, summary, vec![])
    }

    fn build_wait_running_result(
        execution_id: &str,
        entry: &SpawnedProcess,
        timeout_secs: u64,
        wait_output: WaitOutput,
    ) -> CallToolResult {
        let running_for = format_elapsed(entry.started_at.elapsed());
        let mut output = String::new();
        render_metadata_header(
            &mut output,
            MetadataHeader {
                execution_id: Some(execution_id),
                status: Some("running"),
                exit_code: None,
                command: Some(&entry.command),
                working_dir: Some(&entry.working_dir),
                stdout_log_path: Some(&entry.stdout_log_path),
                stderr_log_path: Some(&entry.stderr_log_path),
                total_lines: Some(wait_output.total_lines),
                total_bytes: Some(wait_output.total_bytes),
                running_for: Some(running_for.clone()),
            },
        );
        let _ = write!(output, "\n{}", wait_output.streams_block);
        let summary = format!(
            "execution_id '{execution_id}' still running after {timeout_secs}s ({running_for} total)"
        );
        Self::build_success_result(output, summary, vec![])
    }

    pub(crate) async fn build_wait_exit_result(
        &self,
        ctx: ExitResultCtx<'_>,
    ) -> Result<CallToolResult, ErrorData> {
        let summary = format!(
            "execution_id '{}' exited with code {}",
            ctx.execution_id, ctx.exit_code
        );
        self.build_exit_result(ctx, summary, "after wait").await
    }

    pub(crate) fn build_spawn_result(ctx: SpawnResultCtx<'_>) -> CallToolResult {
        let mut output = String::new();
        render_metadata_header(
            &mut output,
            MetadataHeader {
                execution_id: Some(ctx.execution_id),
                status: Some("spawned"),
                exit_code: None,
                command: Some(ctx.command),
                working_dir: Some(ctx.working_dir),
                stdout_log_path: Some(ctx.stdout_log_path),
                stderr_log_path: Some(ctx.stderr_log_path),
                total_lines: None,
                total_bytes: None,
                running_for: None,
            },
        );
        let summary = format!("spawned {}", ctx.execution_id);
        Self::build_success_result(output, summary, vec![])
    }
}

/// Create the file a spawned command's stdout or stderr is redirected into.
fn create_log_file(path: &Path, stream: &str) -> Result<std::fs::File, ErrorData> {
    std::fs::File::create(path).map_err(|err| {
        internal_error(format!(
            "failed to create {stream} log file '{}': {err}",
            path.display()
        ))
    })
}

/// Compact elapsed time for the still-running header: `7s`, `4m12s`, `1h02m05s`.
pub(crate) fn format_elapsed(elapsed: std::time::Duration) -> String {
    let secs = elapsed.as_secs();
    let (hours, minutes, seconds) = (secs / 3600, secs / 60 % 60, secs % 60);
    if hours > 0 {
        format!("{hours}h{minutes:02}m{seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

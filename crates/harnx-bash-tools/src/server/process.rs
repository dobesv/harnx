// Auto-split from server.rs / handlers.rs for cohesion. See server/mod.rs.
use super::*;

impl BashServer {
    // -----------------------------------------------------------------------
    // spawn
    // -----------------------------------------------------------------------

    pub(crate) async fn spawn_impl(
        &self,
        params: SpawnCommandParams,
    ) -> Result<CallToolResult, ErrorData> {
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

        let mut command = self
            .build_command(
                CommandBuildCtx {
                    command: &params.command,
                    working_dir: &working_dir,
                    exec_dir: &exec_dir,
                    env: params.env.as_ref(),
                },
                Stdio::from(stdout_file),
                Stdio::from(stderr_file),
            )
            .await?;
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
            snapshot_decision: snapshot_decision.clone(),
        };

        self.store_spawned_process(execution_id.clone(), entry)
            .await;

        Ok(Self::build_spawn_result(SpawnResultCtx {
            execution_id: &execution_id,
            command: &params.command,
            working_dir: &working_dir,
            stdout_log_path: &stdout_log_path,
            stderr_log_path: &stderr_log_path,
        }))
    }

    // -----------------------------------------------------------------------
    // wait
    // -----------------------------------------------------------------------

    pub(crate) async fn wait_impl(&self, params: WaitParams) -> Result<CallToolResult, ErrorData> {
        let timeout_secs = params.timeout_secs.unwrap_or(120);
        let truncate_opts = Self::truncate_opts_from(
            params.head_lines,
            params.tail_lines,
            params.max_output_bytes,
        );

        let (
            mut child,
            command,
            working_dir,
            stdout_log_path,
            stderr_log_path,
            before_snap_ids,
            snapshot_decision,
        ) = {
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
                entry.snapshot_decision,
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
                self.build_wait_exit_result(ExitResultCtx {
                    execution_id: &params.execution_id,
                    command: &command,
                    working_dir: &working_dir,
                    stdout_log_path: &stdout_log_path,
                    stderr_log_path: &stderr_log_path,
                    total_lines,
                    total_bytes,
                    exit_code: status.code().unwrap_or(-1),
                    streams_block,
                    before_snaps: &before_snap_ids,
                    snapshot_decision: &snapshot_decision,
                })
                .await
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
                        snapshot_decision,
                    },
                );

                let mut output = String::new();
                render_metadata_header(
                    &mut output,
                    MetadataHeader {
                        execution_id: Some(&params.execution_id),
                        status: Some("running"),
                        exit_code: None,
                        command: Some(&command),
                        working_dir: Some(&working_dir),
                        stdout_log_path: Some(&stdout_log_path),
                        stderr_log_path: Some(&stderr_log_path),
                        total_lines: Some(total_lines),
                        total_bytes: Some(total_bytes),
                    },
                );
                let _ = write!(output, "\n{streams_block}");
                let summary = format!(
                    "execution_id '{}' still running after {}s",
                    params.execution_id, timeout_secs
                );
                Ok(Self::build_success_result(output, summary, vec![]))
            }
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
            },
        );
        let _ = write!(output, "\n\nsignal: {}", signal);
        let summary = format!("sent {} to {}", signal, execution_id);
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
            },
        );
        let summary = format!("spawned {}", ctx.execution_id);
        Self::build_success_result(output, summary, vec![])
    }
}

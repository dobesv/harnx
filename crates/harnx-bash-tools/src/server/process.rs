// Auto-split from server.rs / handlers.rs for cohesion. See server/mod.rs.
use super::exec::resolve_command_timeout;
use super::*;
use tokio_util::sync::CancellationToken;

/// A wait on a tracked process: what to filter, how long to hold the call,
/// and the signal that a new message arrived for the caller, which ends it
/// early.
struct WaitRequest {
    execution_id: String,
    options: WaitOptions,
    /// When the wait gives up on its own and why: its own `seconds`, or the
    /// minute it leaves before the caller's run deadline, whichever is first.
    /// `None` for a forever wait with no caller deadline.
    stop_at: Option<(tokio::time::Instant, WaitEnd)>,
    new_message: CancellationToken,
}

impl WaitRequest {
    /// A wait for the running call.
    fn new(execution_id: String, options: WaitOptions, limit: WaitLimit) -> Self {
        let now = tokio::time::Instant::now();
        let own = match limit {
            WaitLimit::Seconds(seconds) => Some((
                now + Duration::from_secs(seconds),
                WaitEnd::Elapsed(seconds),
            )),
            WaitLimit::Forever => None,
        };
        let caller = crate::toolset::invocation_wait_bound()
            .map(|bound| (now + bound, WaitEnd::CallerDeadline));
        let stop_at = match (own, caller) {
            (Some(own), Some(caller)) => Some(if caller.0 <= own.0 { caller } else { own }),
            (own, caller) => own.or(caller),
        };
        Self {
            execution_id,
            options,
            stop_at,
            new_message: crate::toolset::invocation_new_message(),
        }
    }

    /// Why a wait that left the process running ended. A new message wins
    /// over a limit it raced, since the caller has something to read either way.
    fn end(&self) -> Option<WaitEnd> {
        if self.new_message.is_cancelled() {
            return Some(WaitEnd::NewMessage);
        }
        self.stop_at.map(|(_, end)| end)
    }
}

/// How a wait that left the process running ended.
#[derive(Clone, Copy)]
enum WaitEnd {
    /// Its fixed `seconds` passed.
    Elapsed(u64),
    /// A new message arrived for the caller.
    NewMessage,
    /// The wait reached the bound it keeps before the caller's run deadline.
    CallerDeadline,
}

impl WaitEnd {
    /// The running result's `stopped_by` line, for an end other than the limit.
    fn stopped_by(self) -> Option<&'static str> {
        match self {
            Self::Elapsed(_) => None,
            Self::NewMessage => Some("new_message"),
            Self::CallerDeadline => Some("caller_deadline"),
        }
    }
}

/// What `wait` and `status` report about a tracked process.
struct StateReport<'a> {
    execution_id: &'a str,
    entry: &'a SpawnedProcess,
    exit: Option<ProcessExit>,
    /// Why the wait that just ended stopped, when there was one.
    waited: Option<WaitEnd>,
    options: &'a WaitOptions,
    grep_regex: Option<Regex>,
}

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
        // Validate everything the nested wait and the deadline need before
        // prepare_exec so a bad argument fails the call with zero side
        // effects: no logs, snapshots, or spawned process to clean up.
        let deadline_secs = resolve_command_timeout(params.timeout_secs)?;
        let wait = params
            .wait
            .as_ref()
            .map(|options| Ok::<_, ErrorData>((options.limit()?, compile_wait_grep(options)?)))
            .transpose()?;
        let prepared = self
            .prepare_exec(
                &params.command,
                params.working_dir.as_deref(),
                params.env.as_ref(),
                "before spawn",
            )
            .await?;
        let (execution_id, entry) = self
            .start_spawned_process(&params, prepared, deadline_secs)
            .await?;

        if let (Some(options), Some((limit, grep_regex))) = (params.wait, wait) {
            let request = WaitRequest::new(execution_id, options, limit);
            return self.wait_for_process(request, grep_regex).await;
        }

        Ok(Self::build_spawn_result(SpawnResultCtx {
            execution_id: &execution_id,
            command: &entry.command,
            working_dir: &entry.working_dir,
            stdout_log_path: &entry.stdout_log_path,
            stderr_log_path: &entry.stderr_log_path,
        }))
    }

    /// Spawns the command into its own log directory, hands it to a
    /// supervisor and tracks it. Returns the new execution ID and entry.
    async fn start_spawned_process(
        &self,
        params: &SpawnCommandParams,
        prepared: ExecPreparation,
        deadline_secs: u64,
    ) -> Result<(String, Arc<SpawnedProcess>), ErrorData> {
        self.ensure_log_dir().await?;
        let exec_dir = self.next_exec_dir()?.keep();
        let stdout_log_path = exec_dir.join("stdout.log");
        let stderr_log_path = exec_dir.join("stderr.log");
        let execution_id = exec_dir.file_name().unwrap().to_string_lossy().into_owned();

        let child = self
            .spawn_background_child(
                CommandBuildCtx {
                    command: &params.command,
                    working_dir: &prepared.working_dir,
                    exec_dir: &exec_dir,
                    env: params.env.as_ref(),
                },
                &stdout_log_path,
                &stderr_log_path,
            )
            .await?;

        let metadata = SpawnMetadata {
            command: params.command.clone(),
            working_dir: prepared.working_dir,
            stdout_log_path,
            stderr_log_path,
            before_snap_ids: prepared.before_snap_ids,
            snapshot_decision: prepared.snapshot_decision,
        };
        let deadline = Duration::from_secs(deadline_secs);
        let entry = SpawnedProcess::start(child, deadline, metadata);
        let entry = self
            .store_spawned_process(execution_id.clone(), entry)
            .await;
        // Before any nested wait: a call interrupted while waiting still
        // tells the caller which process it started.
        crate::toolset::record_partial_result(entry.partial_result(&execution_id)).await;
        Ok((execution_id, entry))
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
        let options = params.options();
        let limit = params.limit()?;
        let grep_regex = compile_wait_grep(&options)?;
        let request = WaitRequest::new(params.execution_id, options, limit);
        self.wait_for_process(request, grep_regex).await
    }

    async fn wait_for_process(
        &self,
        request: WaitRequest,
        grep_regex: Option<Regex>,
    ) -> Result<CallToolResult, ErrorData> {
        // The entry is shared, not taken: a dropped wait leaves the process
        // tracked, and concurrent waits and terminates all see it.
        let entry = self.get_spawned_entry(&request.execution_id).await?;
        let exit = entry.wait_for_exit(&request).await?;
        self.report_state(StateReport {
            execution_id: &request.execution_id,
            entry: &entry,
            exit,
            waited: request.end(),
            options: &request.options,
            grep_regex,
        })
        .await
    }

    // -----------------------------------------------------------------------
    // status
    // -----------------------------------------------------------------------

    /// Reports the state as of now, without waiting for anything.
    pub(crate) async fn status_impl(
        &self,
        params: StatusParams,
    ) -> Result<CallToolResult, ErrorData> {
        let options = params.options();
        let grep_regex = compile_wait_grep(&options)?;
        let entry = self.get_spawned_entry(&params.execution_id).await?;
        let exit = entry.recorded_exit(&params.execution_id)?;
        self.report_state(StateReport {
            execution_id: &params.execution_id,
            entry: &entry,
            exit,
            waited: None,
            options: &options,
            grep_regex,
        })
        .await
    }

    /// The running or finished result shared by `wait` and `status`. `waited`
    /// is why the wait that just ended stopped, when there was one.
    async fn report_state(&self, report: StateReport<'_>) -> Result<CallToolResult, ErrorData> {
        let StateReport {
            execution_id,
            entry,
            exit,
            waited,
            options,
            grep_regex,
        } = report;
        let output =
            Self::read_wait_output(execution_id, entry, options, grep_regex.as_ref()).await;
        Ok(match exit {
            Some(exit) => {
                let kind = entry.exit_kind(&exit);
                let ctx = entry.exit_result_ctx(execution_id, exit, output);
                self.build_wait_exit_result(ctx, kind, &entry.exit_diff)
                    .await
            }
            None => Self::build_wait_running_result(execution_id, entry, waited, output),
        })
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

    pub(crate) async fn terminate_impl(
        &self,
        params: TerminateParams,
    ) -> Result<CallToolResult, ErrorData> {
        let normalized = params.signal.as_deref().unwrap_or("SIGTERM").to_uppercase();
        let control = Self::parse_signal(&normalized)?;
        let entry = self.get_spawned_entry(&params.execution_id).await?;
        // The supervisor drops its receiver once it has recorded the exit.
        entry
            .control
            .send(control)
            .map_err(|_| entry.ended_error(&params.execution_id))?;
        Ok(Self::build_terminate_result(
            &params.execution_id,
            &normalized,
            &entry,
        ))
    }

    /// `SIGKILL` goes through `start_kill`, which kills the whole process
    /// group the same way the deadline does.
    #[cfg(unix)]
    pub(crate) fn parse_signal(name: &str) -> Result<ProcessControl, ErrorData> {
        match name {
            "SIGTERM" | "TERM" => Ok(ProcessControl::Signal(libc::SIGTERM)),
            "SIGKILL" | "KILL" => Ok(ProcessControl::Kill),
            "SIGINT" | "INT" => Ok(ProcessControl::Signal(libc::SIGINT)),
            "SIGHUP" | "HUP" => Ok(ProcessControl::Signal(libc::SIGHUP)),
            other => Err(ErrorData::invalid_params(
                format!("unsupported signal: {other}"),
                None,
            )),
        }
    }

    /// Windows has no signals: every one ends the process's job object.
    #[cfg(windows)]
    pub(crate) fn parse_signal(_name: &str) -> Result<ProcessControl, ErrorData> {
        Ok(ProcessControl::Kill)
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
                stopped_by: None,
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
        waited: Option<WaitEnd>,
        wait_output: WaitOutput,
    ) -> CallToolResult {
        let running_for = format_elapsed(entry.started_at.elapsed());
        let mut output = String::new();
        render_metadata_header(
            &mut output,
            MetadataHeader {
                execution_id: Some(execution_id),
                status: Some("running"),
                stopped_by: waited.and_then(WaitEnd::stopped_by),
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
        let summary = match waited {
            Some(WaitEnd::Elapsed(seconds)) => format!(
                "execution_id '{execution_id}' still running after {seconds}s ({running_for} total)"
            ),
            Some(WaitEnd::NewMessage) => format!(
                "execution_id '{execution_id}' still running; a new message arrived ({running_for} total)"
            ),
            Some(WaitEnd::CallerDeadline) => format!(
                "execution_id '{execution_id}' still running; the run deadline is near ({running_for} total)"
            ),
            None => format!("execution_id '{execution_id}' still running ({running_for} total)"),
        };
        Self::build_success_result(output, summary, vec![])
    }

    /// Every wait that sees the exit reports the same diff: the first one
    /// takes the after-exit snapshot, later ones reuse it.
    async fn build_wait_exit_result(
        &self,
        ctx: ExitResultCtx<'_>,
        kind: ExitKind,
        exit_diff: &OnceCell<Vec<String>>,
    ) -> CallToolResult {
        let summary = match kind {
            // Background wait paths cannot be interrupted by a new message.
            ExitKind::Interrupted => "stopped_by: new_message".to_string(),
            ExitKind::TimedOut { deadline_secs } => format!(
                "execution_id '{}' killed after its {deadline_secs}s deadline",
                ctx.execution_id
            ),
            ExitKind::Exited => format!(
                "execution_id '{}' exited with code {}",
                ctx.execution_id, ctx.exit_code
            ),
        };
        let diff_parts = exit_diff
            .get_or_init(|| {
                self.diff_snapshots(
                    ctx.before_snaps,
                    ctx.snapshot_decision,
                    ctx.working_dir,
                    "after wait",
                )
            })
            .await
            .clone();
        Self::exit_result_with_diff(ctx, kind, summary, diff_parts)
    }

    pub(crate) fn build_spawn_result(ctx: SpawnResultCtx<'_>) -> CallToolResult {
        let mut output = String::new();
        render_metadata_header(
            &mut output,
            MetadataHeader {
                execution_id: Some(ctx.execution_id),
                status: Some("spawned"),
                stopped_by: None,
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

impl SpawnedProcess {
    /// Hands `child` to a supervisor that kills its process group once
    /// `deadline` passes, and starts the runtime clock.
    pub(crate) fn start(
        child: Box<dyn ChildWrapper>,
        deadline: Duration,
        metadata: SpawnMetadata,
    ) -> Self {
        let (exit, control) = spawn_supervisor(child, deadline);
        let deadline_secs = deadline.as_secs();
        let SpawnMetadata {
            command,
            working_dir,
            stdout_log_path,
            stderr_log_path,
            before_snap_ids,
            snapshot_decision,
        } = metadata;
        Self {
            command,
            working_dir,
            stdout_log_path,
            stderr_log_path,
            before_snap_ids,
            snapshot_decision,
            started_at: std::time::Instant::now(),
            deadline_secs,
            exit,
            control,
            exit_diff: Arc::default(),
        }
    }

    /// Waits up to the request's limit, or until a new message arrives for
    /// the caller, for the supervisor to record an exit, then reports the
    /// state as of now, so a wait that ends for any reason still answers.
    async fn wait_for_exit(&self, request: &WaitRequest) -> Result<Option<ProcessExit>, ErrorData> {
        let mut exit = self.exit.clone();
        let limit = async {
            match request.stop_at {
                Some((at, _)) => tokio::time::sleep_until(at).await,
                // The process's own deadline ends it, and so the wait.
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = exit.wait_for(Option::is_some) => {}
            _ = limit => {}
            _ = request.new_message.cancelled() => {}
        }
        self.recorded_exit(&request.execution_id)
    }

    /// What `spawn` records before it waits, so a call that does not
    /// succeed still names the process it started.
    fn partial_result(&self, execution_id: &str) -> Value {
        json!({
            "execution_id": execution_id,
            "stdout_log_path": self.stdout_log_path.display().to_string(),
            "stderr_log_path": self.stderr_log_path.display().to_string(),
        })
    }

    /// The exit recorded so far, or the error that made the supervisor give
    /// up. That error stays recorded, so every later call reports it too.
    fn recorded_exit(&self, execution_id: &str) -> Result<Option<ProcessExit>, ErrorData> {
        let exit = self.exit.borrow().clone();
        match exit {
            Some(ProcessExit {
                wait_error: Some(error),
                ..
            }) => Err(internal_error(format!(
                "failed waiting for execution_id '{execution_id}': {error}"
            ))),
            exit => Ok(exit),
        }
    }

    /// Why a control message found the supervisor gone.
    fn ended_error(&self, execution_id: &str) -> ErrorData {
        self.recorded_exit(execution_id).err().unwrap_or_else(|| {
            ErrorData::invalid_params(
                format!("execution_id '{execution_id}' has already exited"),
                None,
            )
        })
    }

    /// Whether `exit` was the supervisor enforcing this process's deadline.
    fn exit_kind(&self, exit: &ProcessExit) -> ExitKind {
        if exit.timed_out {
            ExitKind::TimedOut {
                deadline_secs: self.deadline_secs,
            }
        } else {
            ExitKind::Exited
        }
    }

    fn exit_result_ctx<'a>(
        &'a self,
        execution_id: &'a str,
        exit: ProcessExit,
        output: WaitOutput,
    ) -> ExitResultCtx<'a> {
        ExitResultCtx {
            execution_id,
            command: &self.command,
            working_dir: &self.working_dir,
            stdout_log_path: &self.stdout_log_path,
            stderr_log_path: &self.stderr_log_path,
            total_lines: output.total_lines,
            total_bytes: output.total_bytes,
            exit_code: exit.code.unwrap_or(-1),
            streams_block: output.streams_block,
            before_snaps: &self.before_snap_ids,
            snapshot_decision: &self.snapshot_decision,
        }
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

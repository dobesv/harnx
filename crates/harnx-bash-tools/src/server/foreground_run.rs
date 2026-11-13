//! Foreground process ownership, output collection, and interruption.
use super::foreground_output::ForegroundOutput;
use super::*;

impl BashServer {
    pub(crate) async fn run_exec_command(
        &self,
        params: &ExecPipelineParams<'_>,
        ctx: CommandBuildCtx<'_>,
        targets: LogTargets<'_>,
    ) -> Result<RunOutcome, ErrorData> {
        let command = self
            .build_command_with_sandbox(
                ctx,
                params.template_sandbox,
                Stdio::piped(),
                Stdio::piped(),
            )
            .await?;
        Self::run_to_completion(
            command,
            params.timeout_secs,
            params.on_user_message,
            targets,
        )
        .await
    }

    /// Wrap, spawn, and run `command` to completion (or timeout), streaming its
    /// stdout/stderr into the provided log files. Returns the exit status,
    /// how it ended, and the captured stdout/stderr as lossy UTF-8.
    async fn run_to_completion(
        mut command: CommandWrap,
        timeout_secs: Option<u64>,
        on_user_message: OnUserMessage,
        targets: LogTargets<'_>,
    ) -> Result<RunOutcome, ErrorData> {
        let LogTargets {
            stdout_file,
            stderr_file,
            stdout_log_path,
            stderr_log_path,
        } = targets;
        command.wrap(KillOnDrop);
        #[cfg(unix)]
        command.wrap(ProcessGroup::leader());
        #[cfg(windows)]
        command.wrap(JobObject);

        let mut child = command
            .spawn()
            .map_err(|err| internal_error(format!("failed to spawn command: {err}")))?;

        let stdout = child
            .stdout()
            .take()
            .ok_or_else(|| internal_error("failed to capture stdout"))?;
        let stderr = child
            .stderr()
            .take()
            .ok_or_else(|| internal_error("failed to capture stderr"))?;

        let stdout_task = tokio::spawn(read_pipe_to_file(stdout, stdout_file));
        let stderr_task = tokio::spawn(read_pipe_to_file(stderr, stderr_file));

        let mut output = ForegroundOutput::new(stdout_task, stderr_task);
        let outcome =
            wait_for_child(child.as_mut(), timeout_secs, on_user_message, &mut output).await;
        if let Err(error) = outcome {
            // Never retry a raw group kill after identity validation failed.
            harnx_toolset::cleanup::unconfirmed(
                "bash child wait failed; process cleanup unconfirmed",
            );
            // Dropping KillOnDrop would retry a signal and release the unreaped identity.
            std::mem::forget(child);
            let (stdout, stderr) = output.finish_bounded().await;
            return Err(internal_error(format!(
                "{error}\nstdout_log_path: {}\nstderr_log_path: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
                stdout_log_path.display(),
                stderr_log_path.display(),
            )));
        }

        output.finish().await;
        let (stdout_str, stderr_str) = output.into_strings()?;
        let (status, kind) = outcome.map_err(|err| internal_error(err.to_string()))?;

        // Sync log files to disk to ensure they're visible to other processes immediately
        if let Ok(f) = tokio::fs::File::open(stdout_log_path).await {
            let _ = f.sync_all().await;
        }
        if let Ok(f) = tokio::fs::File::open(stderr_log_path).await {
            let _ = f.sync_all().await;
        }

        Ok(RunOutcome {
            status,
            kind,
            stdout_str,
            stderr_str,
        })
    }
}

async fn wait_for_child(
    child: &mut dyn ChildWrapper,
    timeout_secs: Option<u64>,
    on_user_message: OnUserMessage,
    output: &mut ForegroundOutput,
) -> anyhow::Result<(std::process::ExitStatus, ExitKind)> {
    let identity = super::foreground_cleanup::ProcessIdentity::capture(child);
    let cancel = crate::toolset::INVOCATION_CANCEL
        .try_with(Clone::clone)
        .unwrap_or_default();
    let new_message = crate::toolset::invocation_new_message();
    let deadline = async {
        match timeout_secs {
            Some(seconds) => tokio::time::sleep(Duration::from_secs(seconds)).await,
            None => std::future::pending::<()>().await,
        }
    };
    let completion = async {
        if on_user_message == OnUserMessage::Terminate {
            // Keep the leader unreaped until EOF. Its reserved PID prevents a
            // group kill from hitting a reused PGID if descendants hold pipes open.
            output.finish().await;
        }
        Ok::<_, anyhow::Error>(child.wait().await?)
    };
    let kind = tokio::select! {
        biased;
        _ = cancel.cancelled() => ExitKind::Exited,
        // Check before waiting for exit so an already-pending message is honored.
        _ = new_message.cancelled(), if on_user_message == OnUserMessage::Terminate => ExitKind::Interrupted,
        result = completion => return Ok((result?, ExitKind::Exited)),
        _ = deadline => ExitKind::TimedOut {
            deadline_secs: timeout_secs.expect("deadline requires a timeout"),
        },
    };
    // The tool server's owner supervisor polls this after the logical reply.
    // Reaping is physical evidence; neither the signal nor a dropped waiter is.
    if let Err(error) = identity.terminate(child).await {
        harnx_toolset::cleanup::unconfirmed(format!("bash process-group cleanup: {error:#}"));
        return Err(error);
    }
    Ok((child.wait().await?, kind))
}

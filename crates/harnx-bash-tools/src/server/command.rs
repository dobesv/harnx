// Auto-split from server.rs / handlers.rs for cohesion. See server/mod.rs.
use super::*;

impl BashServer {
    #[cfg(unix)]
    pub(crate) fn build_sandbox_args(
        &self,
        working_dir: &Path,
        roots: &[PathBuf],
    ) -> Vec<OsString> {
        let mut acc = SandboxAcc::new(build_default_sandbox_args(&self.inner.sandbox_config));

        acc.ensure_working_dir_readable(working_dir);
        for root in roots {
            push_root_write_exec(root, &mut acc.args, &mut acc.writable);
        }

        let mut args = acc.into_args();
        for (key, value) in self.build_child_env() {
            args.push(OsString::from("--env"));
            args.push(OsString::from(format!("{key}={value}")));
        }

        args
    }

    pub(crate) async fn write_script_file(
        &self,
        exec_dir: &Path,
        command: &str,
    ) -> Result<PathBuf, ErrorData> {
        let ext = shebang_script_ext(command);
        let script_path = exec_dir.join(format!("script.{ext}"));
        tokio::fs::write(&script_path, command.as_bytes())
            .await
            .map_err(|e| internal_error(format!("failed to write script file: {e}")))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
                .await
                .map_err(|e| internal_error(format!("failed to chmod script: {e}")))?;
        }
        Ok(script_path)
    }

    #[cfg(unix)]
    pub(crate) async fn build_sandbox_command(
        &self,
        spec: SandboxCommandSpec<'_>,
        stdout: Stdio,
        stderr: Stdio,
    ) -> Result<CommandWrap, ErrorData> {
        let SandboxCommandSpec {
            working_dir,
            exec_dir,
            command,
            roots,
            extra_env,
        } = spec;
        let mut sb_args = self.build_sandbox_args(working_dir, roots);
        sb_args.push(OsString::from("--working-dir"));
        sb_args.push(working_dir.as_os_str().to_owned());
        if let Some(extra_env) = extra_env {
            for (key, value) in extra_env {
                sb_args.push(OsString::from("--env"));
                sb_args.push(OsString::from(format!("{key}={value}")));
            }
        }
        sb_args.push(OsString::from("--"));
        if let Some((interp, shebang_args)) = parse_shebang(command) {
            let script_path = self.write_script_file(exec_dir, command).await?;
            if interp.is_absolute() {
                if let Some(interp_dir) = interp.parent() {
                    let dir_str = interp_dir.to_string_lossy();
                    if !SYSTEM_EXEC_PATHS.iter().any(|p| *p == dir_str.as_ref()) {
                        sb_args.push(OsString::from("--exec"));
                        sb_args.push(interp_dir.as_os_str().to_owned());
                    }
                }
            }
            sb_args.push(interp.as_os_str().to_owned());
            for arg in shebang_args {
                sb_args.push(OsString::from(arg));
            }
            sb_args.push(script_path.as_os_str().to_owned());
        } else {
            sb_args.push(OsString::from("bash"));
            sb_args.push(OsString::from("-c"));
            sb_args.push(OsString::from(command));
        }
        let sandbox_run_path = self.inner.sandbox_config.sandbox_run_path.clone();
        Ok(CommandWrap::with_new(sandbox_run_path, |command_wrap| {
            command_wrap
                .args(&sb_args)
                .current_dir(working_dir)
                .stdin(Stdio::null())
                .stdout(stdout)
                .stderr(stderr);
        }))
    }

    pub(crate) async fn build_local_command(
        &self,
        working_dir: &Path,
        exec_dir: &Path,
        command: &str,
        extra_env: &HashMap<String, String>,
        stdout: Stdio,
        stderr: Stdio,
    ) -> Result<CommandWrap, ErrorData> {
        let child_env = self.build_child_env();
        if let Some((interp, shebang_args)) = parse_shebang(command) {
            let script_path = self.write_script_file(exec_dir, command).await?;
            Ok(CommandWrap::with_new(&interp, |command_wrap| {
                command_wrap
                    .args(&shebang_args)
                    .arg(&script_path)
                    .current_dir(working_dir)
                    .stdin(Stdio::null());
                command_wrap.env_clear();
                command_wrap.envs(child_env.iter().map(|(k, v)| (k, v)));
                command_wrap.envs(extra_env);
                command_wrap.stdout(stdout).stderr(stderr);
            }))
        } else {
            Ok(CommandWrap::with_new("bash", |command_wrap| {
                command_wrap
                    .args(["-c", command])
                    .current_dir(working_dir)
                    .stdin(Stdio::null());
                command_wrap.env_clear();
                command_wrap.envs(child_env.iter().map(|(k, v)| (k, v)));
                command_wrap.envs(extra_env);
                command_wrap.stdout(stdout).stderr(stderr);
            }))
        }
    }

    /// Build the child `CommandWrap` for exec/spawn, selecting the sandboxed
    /// or local builder based on configuration. `stdout`/`stderr` are the
    /// per-tool output destinations (piped for exec, log files for spawn).
    pub(crate) async fn build_command(
        &self,
        ctx: CommandBuildCtx<'_>,
        stdout: Stdio,
        stderr: Stdio,
    ) -> Result<CommandWrap, ErrorData> {
        #[cfg(unix)]
        let use_sandbox = self.inner.sandbox_config.enabled;
        #[cfg(not(unix))]
        let use_sandbox = false;

        if use_sandbox {
            #[cfg(unix)]
            {
                let roots_guard = self.inner.roots.read().await;
                let command = self
                    .build_sandbox_command(
                        SandboxCommandSpec {
                            working_dir: ctx.working_dir,
                            exec_dir: ctx.exec_dir,
                            command: ctx.command,
                            roots: &roots_guard,
                            extra_env: ctx.env,
                        },
                        stdout,
                        stderr,
                    )
                    .await?;
                drop(roots_guard);
                Ok(command)
            }
            #[cfg(not(unix))]
            unreachable!()
        } else {
            let extra_env = ctx.env.cloned().unwrap_or_default();
            self.build_local_command(
                ctx.working_dir,
                ctx.exec_dir,
                ctx.command,
                &extra_env,
                stdout,
                stderr,
            )
            .await
        }
    }

    /// Wrap, spawn, and run `command` to completion (or timeout), streaming its
    /// stdout/stderr into the provided log files. Returns the exit status,
    /// whether it timed out, and the captured stdout/stderr as lossy UTF-8.
    pub(crate) async fn run_to_completion(
        mut command: CommandWrap,
        timeout_secs: u64,
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

        let timeout = Duration::from_secs(timeout_secs);
        let (status, timed_out) = match tokio::time::timeout(timeout, child.wait()).await {
            Ok(Ok(status)) => (Some(status), false),
            Ok(Err(err)) => {
                return Err(internal_error(format!("failed waiting for command: {err}")));
            }
            Err(_) => {
                child.start_kill().map_err(|err| {
                    internal_error(format!("failed to kill command after timeout: {err}"))
                })?;
                match child.wait().await {
                    Ok(status) => (Some(status), true),
                    Err(err) => {
                        return Err(internal_error(format!(
                            "failed waiting for killed command: {err}"
                        )));
                    }
                }
            }
        };

        let stdout_bytes = join_pipe(stdout_task, "stdout").await?;
        let stderr_bytes = join_pipe(stderr_task, "stderr").await?;

        // Sync log files to disk to ensure they're visible to other processes immediately
        if let Ok(f) = tokio::fs::File::open(stdout_log_path).await {
            let _ = f.sync_all().await;
        }
        if let Ok(f) = tokio::fs::File::open(stderr_log_path).await {
            let _ = f.sync_all().await;
        }

        Ok(RunOutcome {
            status,
            timed_out,
            stdout_str: String::from_utf8_lossy(&stdout_bytes).into_owned(),
            stderr_str: String::from_utf8_lossy(&stderr_bytes).into_owned(),
        })
    }
}

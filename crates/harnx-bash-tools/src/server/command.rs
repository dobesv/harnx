// Auto-split from server.rs / handlers.rs for cohesion. See server/mod.rs.
use super::*;

/// Expands template grant paths against the server process's ambient
/// environment. Callers run this while loading templates, before child
/// processes clear their environment.
pub(crate) fn expand_path(raw: &str) -> anyhow::Result<PathBuf> {
    let expanded = expand_env_references(raw)?;
    let Some(rest) = expanded.strip_prefix('~') else {
        return Ok(PathBuf::from(expanded));
    };

    let home = match std::env::var("HOME") {
        Ok(home) => home,
        Err(std::env::VarError::NotPresent) => {
            std::env::var("USERPROFILE").map_err(|err| match err {
                std::env::VarError::NotPresent => anyhow::anyhow!(
                    "cannot expand '~' in path '{raw}': neither HOME nor USERPROFILE is set"
                ),
                std::env::VarError::NotUnicode(_) => anyhow::anyhow!(
                    "cannot expand '~' in path '{raw}': USERPROFILE is not valid UTF-8"
                ),
            })?
        }
        Err(std::env::VarError::NotUnicode(_)) => {
            anyhow::bail!("cannot expand '~' in path '{raw}': HOME is not valid UTF-8")
        }
    };
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    Ok(PathBuf::from(home).join(rest))
}

fn expand_env_references(raw: &str) -> anyhow::Result<String> {
    let mut expanded = String::with_capacity(raw.len());
    let mut index = 0;

    while let Some(relative_dollar) = raw[index..].find('$') {
        let dollar = index + relative_dollar;
        expanded.push_str(&raw[index..dollar]);
        let suffix = &raw[dollar + 1..];

        let (name, consumed) = if let Some(braced) = suffix.strip_prefix('{') {
            let Some(end) = braced.find('}') else {
                expanded.push_str(&raw[dollar..]);
                return Ok(expanded);
            };
            let name = &braced[..end];
            if name.is_empty() {
                anyhow::bail!("empty environment variable reference in path '{raw}'");
            }
            (name, end + 3)
        } else {
            let name_len = suffix
                .bytes()
                .take_while(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
                .count();
            let name = &suffix[..name_len];
            let starts_validly = name
                .as_bytes()
                .first()
                .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_');
            if !starts_validly {
                expanded.push('$');
                index = dollar + 1;
                continue;
            }
            (name, name_len + 1)
        };

        let value = std::env::var(name).map_err(|err| match err {
            std::env::VarError::NotPresent => anyhow::anyhow!(
                "environment variable '{name}' is not set while expanding path '{raw}'"
            ),
            std::env::VarError::NotUnicode(_) => anyhow::anyhow!(
                "environment variable '{name}' is not valid UTF-8 while expanding path '{raw}'"
            ),
        })?;
        expanded.push_str(&value);
        index = dollar + consumed;
    }

    expanded.push_str(&raw[index..]);
    Ok(expanded)
}

#[cfg(unix)]
fn append_sandbox_command_args(args: &mut Vec<OsString>, spec: &SandboxCommandSpec<'_>) {
    // The server writes shebang scripts and execution logs here. Grant the
    // helper access to its own private directory even when callers use a
    // deliberately narrow filesystem allowlist.
    args.push(OsString::from("--write"));
    args.push(spec.exec_dir.as_os_str().to_owned());
    args.push(OsString::from("--exec"));
    args.push(spec.exec_dir.as_os_str().to_owned());
    args.push(OsString::from("--working-dir"));
    args.push(spec.working_dir.as_os_str().to_owned());
    for name in &spec.pass_env {
        args.push(OsString::from("--env"));
        args.push(OsString::from(name));
    }
    // Explicit template env and validated parameter bindings come last so they
    // win if a template also allows the same ambient variable through.
    if let Some(extra_env) = spec.extra_env {
        for (key, value) in extra_env {
            args.push(OsString::from("--env"));
            args.push(OsString::from(format!("{key}={value}")));
        }
    }
    for path in &spec.read_paths {
        args.push(OsString::from("--read"));
        args.push(path.as_os_str().to_owned());
    }
    for path in &spec.write_paths {
        args.push(OsString::from("--write"));
        args.push(path.as_os_str().to_owned());
    }
    if spec.no_network {
        args.push(OsString::from("--no-network"));
    }
}

impl BashServer {
    #[cfg(unix)]
    pub(crate) fn build_sandbox_args(&self, _working_dir: &Path) -> Vec<OsString> {
        let mut args = build_default_sandbox_args(&self.inner.sandbox_config);
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
        let mut sb_args = self.build_sandbox_args(spec.working_dir);
        append_sandbox_command_args(&mut sb_args, &spec);
        let SandboxCommandSpec {
            working_dir,
            exec_dir,
            command,
            ..
        } = spec;
        sb_args.push(OsString::from("--"));
        if let Some((interp, shebang_args)) = parse_shebang(command) {
            let script_path = self.write_script_file(exec_dir, command).await?;
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
        // The sandbox reports its own setup failures from inside a PID
        // namespace, where the message carries no indication of which path or
        // binary was missing. Logging the invocation makes the failure
        // reproducible outside harnx by pasting it into a shell.
        log::debug!(
            "sandbox exec: {}",
            redacted_invocation(&sandbox_run_path, &sb_args)
        );
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
        self.build_command_with_sandbox(ctx, None, stdout, stderr)
            .await
    }

    pub(crate) async fn build_command_with_sandbox(
        &self,
        ctx: CommandBuildCtx<'_>,
        template_sandbox: Option<TemplateSandbox<'_>>,
        stdout: Stdio,
        stderr: Stdio,
    ) -> Result<CommandWrap, ErrorData> {
        #[cfg(not(unix))]
        let _ = &template_sandbox;
        #[cfg(unix)]
        let use_sandbox = self.inner.sandbox_config.enabled
            && template_sandbox
                .as_ref()
                .is_none_or(|sandbox| sandbox.enabled);
        #[cfg(not(unix))]
        let use_sandbox = false;

        if use_sandbox {
            #[cfg(unix)]
            {
                let (read_paths, write_paths, pass_env, no_network) = template_sandbox
                    .map(|sandbox| {
                        (
                            sandbox.read_paths.to_vec(),
                            sandbox.write_paths.to_vec(),
                            sandbox.pass_env.to_vec(),
                            sandbox.no_network,
                        )
                    })
                    .unwrap_or_default();
                self.build_sandbox_command(
                    SandboxCommandSpec {
                        working_dir: ctx.working_dir,
                        exec_dir: ctx.exec_dir,
                        command: ctx.command,
                        extra_env: ctx.env,
                        read_paths,
                        write_paths,
                        pass_env,
                        no_network,
                    },
                    stdout,
                    stderr,
                )
                .await
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
        timeout_secs: Option<u64>,
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

        let (status, timed_out) = wait_for_child(child.as_mut(), timeout_secs)
            .await
            .map_err(|err| internal_error(err.to_string()))?;

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

async fn wait_for_child(
    child: &mut dyn ChildWrapper,
    timeout_secs: Option<u64>,
) -> anyhow::Result<(std::process::ExitStatus, bool)> {
    let Some(timeout_secs) = timeout_secs else {
        return Ok((child.wait().await?, false));
    };

    match tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait()).await {
        Ok(result) => Ok((result?, false)),
        Err(_) => {
            child.start_kill()?;
            Ok((child.wait().await?, true))
        }
    }
}

/// Renders the invocation as a line that can be pasted into a shell, with
/// `--env` values replaced by a placeholder.
///
/// Those values carry API keys and tokens. Quoting is not redaction: it only
/// escapes syntax, so a token would land in the log verbatim. The command
/// itself is kept, since reproducing it is the whole point of the line and it
/// is the caller's own, already visible to anyone who can read this log.
#[cfg(unix)]
fn redacted_invocation(program: &Path, args: &[OsString]) -> String {
    fn quote(value: &str) -> String {
        shell_words::quote(value).into_owned()
    }

    let mut out = vec![quote(&program.to_string_lossy())];
    let mut past_separator = false;
    let mut env_value_next = false;
    for arg in args {
        if std::mem::take(&mut env_value_next) {
            out.push(quote(&redact_env_value(arg)));
            continue;
        }
        // A `--env` appearing after `--` belongs to the sandboxed command.
        if !past_separator {
            past_separator = arg == "--";
            env_value_next = arg == "--env";
        }
        out.push(quote(&arg.to_string_lossy()));
    }
    out.join(" ")
}

/// `--env` also accepts a bare name, which names a variable to pass through
/// from the ambient environment and so carries no value to hide.
#[cfg(unix)]
fn redact_env_value(arg: &std::ffi::OsStr) -> String {
    let raw = arg.to_string_lossy();
    match raw.split_once('=') {
        Some((name, _)) => format!("{name}=<redacted>"),
        None => raw.into_owned(),
    }
}

#[cfg(all(test, unix))]
mod sandbox_capability_tests {
    use super::*;

    fn spec<'a>(working_dir: &'a Path, exec_dir: &'a Path) -> SandboxCommandSpec<'a> {
        SandboxCommandSpec {
            working_dir,
            exec_dir,
            command: "true",
            extra_env: None,
            read_paths: Vec::new(),
            write_paths: Vec::new(),
            pass_env: Vec::new(),
            no_network: false,
        }
    }

    fn contains_pair(args: &[OsString], flag: &str, value: &str) -> bool {
        args.windows(2)
            .any(|pair| pair[0] == flag && pair[1] == value)
    }

    #[test]
    fn sandbox_capabilities_emit_read_env_and_no_network_args() {
        let mut spec = spec(Path::new("/workspace"), Path::new("/tmp/execution"));
        spec.read_paths = vec![PathBuf::from("/home/x/.config/gh")];
        spec.pass_env = vec!["GH_TOKEN".to_string()];
        spec.no_network = true;
        let mut args = Vec::new();

        append_sandbox_command_args(&mut args, &spec);

        assert!(contains_pair(&args, "--read", "/home/x/.config/gh"));
        assert!(contains_pair(&args, "--env", "GH_TOKEN"));
        assert!(!args.iter().any(|arg| arg == "GH_TOKEN="));
        assert!(args.iter().any(|arg| arg == "--no-network"));
    }

    #[test]
    fn default_sandbox_capabilities_emit_no_additional_args() {
        let spec = spec(Path::new("/workspace"), Path::new("/tmp/execution"));
        let mut args = Vec::new();

        append_sandbox_command_args(&mut args, &spec);

        assert!(!args.iter().any(|arg| arg == "--read"));
        assert!(!args.iter().any(|arg| arg == "--env"));
        assert!(!args.iter().any(|arg| arg == "--no-network"));
        assert_eq!(args.iter().filter(|arg| *arg == "--write").count(), 1);
    }

    #[test]
    fn explicit_env_value_follows_same_named_passthrough() {
        let mut spec = spec(Path::new("/workspace"), Path::new("/tmp/execution"));
        let extra_env = HashMap::from([("TOKEN".to_string(), "bound".to_string())]);
        spec.pass_env = vec!["TOKEN".to_string()];
        spec.extra_env = Some(&extra_env);
        let mut args = Vec::new();

        append_sandbox_command_args(&mut args, &spec);

        let passthrough = args.iter().position(|arg| arg == "TOKEN").unwrap();
        let explicit = args.iter().position(|arg| arg == "TOKEN=bound").unwrap();
        assert!(passthrough < explicit);
    }

    #[test]
    fn expand_path_resolves_home_and_preserves_plain_paths() {
        let home = PathBuf::from(std::env::var("HOME").expect("HOME should be set for test"));

        assert_eq!(expand_path("~/foo").unwrap(), home.join("foo"));
        assert_eq!(expand_path("$HOME/foo").unwrap(), home.join("foo"));
        assert_eq!(expand_path("${HOME}/foo").unwrap(), home.join("foo"));
        assert_eq!(expand_path("/abs/x").unwrap(), PathBuf::from("/abs/x"));
    }

    #[test]
    fn expand_path_uses_userprofile_when_home_is_unset() {
        let _env_guard = crate::test_support::env_lock();
        let _home = crate::test_support::EnvVar::unset("HOME");
        let profile = std::env::temp_dir().join("harnx-user-profile");
        let _profile = crate::test_support::EnvVar::set(
            "USERPROFILE",
            profile.to_str().expect("test profile should be UTF-8"),
        );

        assert_eq!(expand_path("~/foo").unwrap(), profile.join("foo"));
    }

    #[test]
    fn expand_path_rejects_unset_environment_variables() {
        const NAME: &str = "DEFINITELY_UNSET_VAR_xyz";
        let _env_guard = crate::test_support::env_lock();
        let _unset = crate::test_support::EnvVar::unset(NAME);

        let error = expand_path("$DEFINITELY_UNSET_VAR_xyz/foo").unwrap_err();

        assert!(
            error.to_string().contains(NAME),
            "unexpected error: {error}"
        );
    }
}

#[cfg(all(test, unix))]
mod redaction_tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn env_values_are_replaced_but_their_names_survive() {
        let line = redacted_invocation(
            Path::new("/usr/bin/harnx-sandbox-run"),
            &args(&["--env", "EXA_API_KEY=secret-token", "--read", "/usr"]),
        );

        assert_eq!(
            line,
            // The placeholder is quoted like any other value, so the line stays
            // safe to paste.
            "/usr/bin/harnx-sandbox-run --env 'EXA_API_KEY=<redacted>' --read /usr"
        );
        assert!(!line.contains("secret-token"));
    }

    #[test]
    fn a_bare_env_name_and_the_command_are_left_alone() {
        let line = redacted_invocation(
            Path::new("/usr/bin/harnx-sandbox-run"),
            &args(&["--env", "PATH", "--", "bash", "-c", "echo --env hi"]),
        );

        assert_eq!(
            line,
            "/usr/bin/harnx-sandbox-run --env PATH -- bash -c 'echo --env hi'"
        );
    }

    #[test]
    fn a_path_needing_quoting_is_quoted() {
        let line = redacted_invocation(Path::new("/opt/my tools/run"), &args(&["--read", "/usr"]));

        assert_eq!(line, "'/opt/my tools/run' --read /usr");
    }
}

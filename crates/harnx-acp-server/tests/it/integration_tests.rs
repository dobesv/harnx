//! Full ACP protocol handshake over stdio. Build workspace binaries first.

#[cfg(test)]
mod tests {
    use anyhow::{ensure, Context, Result};
    use harnx_core::child_process::ChildProcessManager;
    use process_wrap::tokio::{ChildWrapper, CommandWrap};
    #[cfg(unix)]
    use process_wrap::tokio::{CommandWrapper, ProcessGroup};
    #[cfg(windows)]
    use process_wrap::tokio::{JobObject, KillOnDrop};
    use serde_json::{json, Value};
    use std::{
        collections::VecDeque,
        io::Write,
        process::Stdio,
        sync::{Arc, OnceLock},
        time::{Duration, Instant},
    };
    use tempfile::TempDir;
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines},
        process::{ChildStdin, ChildStdout, Command},
    };
    use tokio_util::task::AbortOnDropHandle;

    const RESPONSE_DEADLINE: Duration = Duration::from_secs(120);
    const CLEANUP_DEADLINE: Duration = Duration::from_secs(10);
    const STDERR_LIMIT: usize = 64 * 1024;

    // Direct writes bypass libtest capture, which is lost when nextest kills a hang.
    fn phase(message: impl std::fmt::Display) {
        static START: OnceLock<Instant> = OnceLock::new();
        let elapsed = START.get_or_init(Instant::now).elapsed();
        let mut stderr = std::io::stderr().lock();
        let _ = writeln!(
            stderr,
            "ACP fixture test_pid={} elapsed_ms={} {message}",
            std::process::id(),
            elapsed.as_millis(),
        );
        let _ = stderr.flush();
    }

    struct TestBodyMarker;

    impl Drop for TestBodyMarker {
        fn drop(&mut self) {
            phase("test body finished (including error/unwind); runtime shutdown next");
        }
    }

    struct FixtureChild(Box<dyn ChildWrapper>);

    impl FixtureChild {
        async fn spawn(manager: &ChildProcessManager, command: Command) -> Result<Self> {
            #[cfg(not(windows))]
            let child = Box::new(
                tokio::time::timeout(RESPONSE_DEADLINE, manager.spawn(command))
                    .await
                    .context("ACP spawn deadline")?
                    .context("start ACP server")?,
            );
            #[cfg(windows)]
            let child = {
                let _ = manager;
                // process-wrap suspends ACP, assigns its job, then resumes it.
                // Descendants can't escape containment before the first request.
                let mut command = CommandWrap::from(command);
                command.wrap(KillOnDrop).wrap(JobObject);
                command.spawn().context("start ACP server in Windows job")?
            };
            Ok(Self(child))
        }

        fn id(&self) -> Option<u32> {
            self.0.id()
        }

        fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
            // Don't use JobObjectChild::wait: it starts an unbounded blocking
            // completion-port wait. Reap the direct child with Tokio's async wait.
            self.0.inner_mut().try_wait()
        }

        async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
            self.0.inner_mut().wait().await
        }
    }

    struct ServerFixture {
        child: FixtureChild,
        pid: Option<u32>,
        stdin: Option<ChildStdin>,
        stdout: Lines<BufReader<ChildStdout>>,
        stderr: Arc<parking_lot::Mutex<VecDeque<u8>>>,
        stderr_task: AbortOnDropHandle<std::io::Result<()>>,
        _manager: ChildProcessManager,
        root: TempDir,
    }

    impl ServerFixture {
        async fn spawn() -> Result<Self> {
            harnx_core::require_nextest();
            phase("sandbox setup begin");
            let root = prepare_sandbox()?;
            phase(format_args!(
                "sandbox setup complete path={}",
                root.path().display()
            ));
            let command = server_command(&root);
            let manager = ChildProcessManager::new();
            phase("managed ACP spawn begin");
            let mut child = FixtureChild::spawn(&manager, command).await?;
            phase(format_args!(
                "managed ACP spawn complete pid={:?}",
                child.id()
            ));
            let stdin = child.0.stdin().take().context("capture ACP stdin")?;
            let stdout =
                BufReader::new(child.0.stdout().take().context("capture ACP stdout")?).lines();
            let mut stderr_pipe = child.0.stderr().take().context("capture ACP stderr")?;
            let stderr = Arc::new(parking_lot::Mutex::new(VecDeque::new()));
            let captured = Arc::clone(&stderr);
            let stderr_task = AbortOnDropHandle::new(tokio::spawn(async move {
                phase("stderr reader begin");
                let mut buffer = [0; 4096];
                loop {
                    let size = stderr_pipe.read(&mut buffer).await?;
                    if size == 0 {
                        phase("stderr reader EOF");
                        return Ok(());
                    }
                    let mut tail = captured.lock();
                    tail.extend(&buffer[..size]);
                    let excess = tail.len().saturating_sub(STDERR_LIMIT);
                    tail.drain(..excess);
                }
            }));
            Ok(Self {
                pid: child.id(),
                child,
                stdin: Some(stdin),
                stdout,
                stderr,
                stderr_task,
                _manager: manager,
                root,
            })
        }

        async fn request(&mut self, id: u64, method: &str, params: Value) -> Result<Value> {
            let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
            let stdin = self.stdin.as_mut().context("ACP stdin closed")?;
            let line = format!("{request}\n");
            phase(format_args!("request {method} id={id} write begin"));
            let result = tokio::time::timeout(RESPONSE_DEADLINE, async {
                stdin.write_all(line.as_bytes()).await?;
                stdin.flush().await?;
                phase(format_args!("request {method} id={id} flushed; read begin"));
                let response = tokio::select! {
                    status = self.child.wait() => {
                        anyhow::bail!("ACP exited during {method} id={id}: {}", status?);
                    }
                    response = self.stdout.next_line() => response?.context("ACP stdout EOF")?,
                };
                phase(format_args!("request {method} id={id} frame={response}"));
                serde_json::from_str(&response)
                    .with_context(|| format!("invalid ACP frame: {response:?}"))
            })
            .await;
            phase(format_args!(
                "request {method} id={id} finished result={result:?}"
            ));
            result
                .with_context(|| format!("ACP {method} id={id} response deadline"))?
                .with_context(|| format!("ACP {method} id={id}"))
        }

        fn diagnostics(&mut self) -> String {
            let status = self.child.try_wait();
            let bytes: Vec<_> = self.stderr.lock().iter().copied().collect();
            format!(
                "ACP pid={:?} status={status:?} sandbox={}\nstderr tail:\n{}",
                self.pid,
                self.root.path().display(),
                String::from_utf8_lossy(&bytes),
            )
        }

        async fn shutdown(&mut self) -> Result<()> {
            phase("shutdown begin");
            // Retire the Windows job before waiting on pipes. The job still
            // contains broker/worker descendants if the ACP owner crashed.
            #[cfg(windows)]
            self.force_stop().await?;
            // Other platforms can let the ACP owner shut down through stdin EOF.
            phase("stdin drop begin");
            self.stdin.take();
            phase("stdin dropped; ACP wait begin");
            let wait = tokio::time::timeout(CLEANUP_DEADLINE, self.child.wait()).await;
            phase(format_args!("ACP wait finished result={wait:?}"));
            let status = match wait {
                Ok(status) => status.context("reap ACP server")?,
                Err(_) => {
                    self.force_stop().await?;
                    anyhow::bail!("ACP server did not exit after stdin EOF");
                }
            };
            self.drain_stderr().await?;
            #[cfg(not(windows))]
            ensure!(status.success(), "ACP exit status: {status}");
            #[cfg(windows)]
            let _ = status;
            phase("shutdown complete");
            Ok(())
        }

        async fn drain_stderr(&mut self) -> Result<()> {
            phase("stderr join begin");
            tokio::time::timeout(CLEANUP_DEADLINE, &mut self.stderr_task)
                .await
                .context("ACP stderr drain deadline")?
                .context("ACP stderr reader task")??;
            phase("stderr join complete");
            Ok(())
        }

        async fn force_stop(&mut self) -> Result<()> {
            phase(format_args!("force stop begin pid={:?}", self.pid));
            // A job owns descendants even after ACP crashes. PID-tree discovery
            // after owner exit cannot find all pipe owners (Windows CI 36946194455).
            #[cfg(windows)]
            let tree_cleanup = self.child.0.start_kill().context("terminate ACP job tree");
            // Still kill/reap the direct child if tree cleanup fails.
            phase("direct child kill/reap begin");
            if self.child.try_wait()?.is_none() {
                self.child
                    .0
                    .inner_mut()
                    .start_kill()
                    .context("kill ACP server")?;
            }
            tokio::time::timeout(CLEANUP_DEADLINE, self.child.wait())
                .await
                .context("ACP kill/reap deadline")??;
            phase("direct child reaped");
            #[cfg(windows)]
            tree_cleanup?;
            phase("force stop complete");
            Ok(())
        }
    }

    fn drop_fixture(fixture: ServerFixture) {
        let ServerFixture {
            child,
            stdin,
            stdout,
            stderr_task,
            _manager,
            root,
            ..
        } = fixture;
        phase("fixture pipe/reader drop begin");
        drop((child, stdin, stdout, stderr_task));
        phase("fixture pipe/reader drop complete; manager drop begin");
        drop(_manager);
        phase("fixture manager drop complete; sandbox drop begin");
        drop(root);
        phase("fixture drop complete");
    }

    fn prepare_sandbox() -> Result<TempDir> {
        let root = tempfile::tempdir()?;
        for directory in ["config/agents", "data", "state", "work"] {
            std::fs::create_dir_all(root.path().join(directory))?;
        }
        std::fs::write(
            root.path().join("config/config.yaml"),
            "save: false\nstream: false\n",
        )?;
        // These tests create sessions but never prompt a model or start tools.
        std::fs::write(
            root.path().join("config/agents/test-agent.md"),
            "---\nuse_tools: []\n---\nACP stdio fixture agent.\n",
        )?;
        Ok(root)
    }

    fn server_command(root: &TempDir) -> Command {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin("harnx-acp-server"));
        // Preserve OS/PATH and NATS_SERVER_BIN, but don't inherit operator routing,
        // config overrides, or worker/tool settings from the test runner.
        for (name, _) in std::env::vars_os() {
            if name
                .to_string_lossy()
                .to_ascii_uppercase()
                .starts_with("HARNX_")
            {
                command.env_remove(name);
            }
        }
        command
            .args(["--agent", "test-agent", "--log-level", "error"])
            .env("HARNX_CONFIG_DIR", root.path().join("config"))
            .env("HARNX_DATA_DIR", root.path().join("data"))
            .env("HARNX_STATE_DIR", root.path().join("state"))
            .env(
                "HARNX_WORKER_BIN",
                assert_cmd::cargo::cargo_bin("harnx-worker"),
            )
            .env("RUST_LOG", "error")
            .current_dir(root.path().join("work"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }

    // Protocol errors return before assertions, so cleanup runs on every exchange failure.
    async fn exchange(mcp_servers: Option<Value>) -> Result<Vec<Value>> {
        let mut fixture = ServerFixture::spawn().await?;
        let result: Result<Vec<Value>> = async {
            let mut responses = vec![
                fixture
                    .request(
                        1,
                        "initialize",
                        json!({"protocolVersion": 1, "clientCapabilities": {}}),
                    )
                    .await?,
            ];
            if let Some(mcp_servers) = mcp_servers {
                let params = json!({
                    "cwd": fixture.root.path().join("work"),
                    "mcpServers": mcp_servers,
                });
                responses.push(fixture.request(2, "session/new", params).await?);
            }
            Ok(responses)
        }
        .await;
        let cleanup = fixture.shutdown().await;
        let diagnostics = format!(
            "{}\nexchange={result:?}\ncleanup={cleanup:?}",
            fixture.diagnostics(),
        );
        phase(&diagnostics);
        drop_fixture(fixture);
        let responses = result.with_context(|| diagnostics.clone())?;
        cleanup.with_context(|| diagnostics.clone())?;
        for response in &responses {
            ensure!(
                response.get("error").is_none(),
                "ACP error response: {response}\n{diagnostics}"
            );
        }
        Ok(responses)
    }

    fn assert_initialized(response: &Value) {
        assert_eq!(response["jsonrpc"], "2.0", "{response}");
        assert_eq!(response["id"], 1, "{response}");
        assert_eq!(response["result"]["protocolVersion"], 1, "{response}");
        assert_eq!(
            response["result"]["agentInfo"]["name"], "harnx",
            "{response}"
        );
        assert_eq!(
            response["result"]["agentCapabilities"]["loadSession"], true,
            "{response}"
        );
        assert_eq!(
            response["result"]["agentCapabilities"]["sessionCapabilities"],
            json!({"list": {}, "resume": {}, "close": {}}),
            "{response}"
        );
    }

    fn assert_new_session_response(response: &Value) {
        assert_eq!(response["jsonrpc"], "2.0", "{response}");
        assert_eq!(response["id"], 2, "{response}");
        let result = &response["result"];
        let session_id = result["sessionId"]
            .as_str()
            .unwrap_or_else(|| panic!("Expected sessionId: {response}"));
        assert!(
            !session_id.is_empty(),
            "Session ID should not be empty: {response}"
        );
        assert!(
            session_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "Session ID should be base64url: {response}"
        );
        assert!(result.get("modes").is_none(), "{response}");
        assert!(result.get("configOptions").is_none(), "{response}");
    }

    #[tokio::test]
    async fn session_new_accepts_empty_mcp_servers() -> Result<()> {
        let _body = TestBodyMarker;
        let responses = exchange(Some(json!([]))).await?;
        assert_initialized(&responses[0]);
        assert_new_session_response(&responses[1]);
        Ok(())
    }

    #[tokio::test]
    async fn session_new_accepts_injected_mcp_servers() -> Result<()> {
        let _body = TestBodyMarker;
        let responses = exchange(Some(json!([{
            "name": "jetbrains-ide", "command": "/usr/bin/false",
            "args": ["--ide-injected"], "env": [],
        }])))
        .await?;
        assert_initialized(&responses[0]);
        assert_new_session_response(&responses[1]);
        Ok(())
    }

    #[tokio::test]
    async fn nats_forced_cleanup_reaps_stdio_child() -> Result<()> {
        let _body = TestBodyMarker;
        let mut fixture = ServerFixture::spawn().await?;
        let result = async {
            fixture
                .request(
                    1,
                    "initialize",
                    json!({
                        "protocolVersion": 1, "clientCapabilities": {},
                    }),
                )
                .await?;
            let cwd = fixture.root.path().join("work");
            fixture
                .request(2, "session/new", json!({"cwd": cwd, "mcpServers": []}))
                .await
        }
        .await;
        let cleanup = async {
            fixture.force_stop().await?;
            fixture.drain_stderr().await?;
            let frame = tokio::time::timeout(CLEANUP_DEADLINE, fixture.stdout.next_line())
                .await
                .context("forced ACP stdout EOF deadline")??;
            ensure!(
                frame.is_none(),
                "unexpected frame after forced cleanup: {frame:?}"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let reaped = fixture.child.try_wait();
        let diagnostics = format!(
            "{}\nexchange={result:?}\ncleanup={cleanup:?}",
            fixture.diagnostics()
        );
        phase(&diagnostics);
        drop_fixture(fixture);
        cleanup.with_context(|| diagnostics.clone())?;
        assert!(reaped?.is_some(), "{diagnostics}");
        let response = result.with_context(|| diagnostics.clone())?;
        assert_new_session_response(&response);
        Ok(())
    }

    const PIPE_PROBE_TEST: &str = "integration_tests::tests::orphan_probe_retains_inherited_pipes";

    fn pipe_probe_command(role: &str, root: &std::path::Path) -> Result<Command> {
        let mut command = Command::new(std::env::current_exe()?);
        command
            .args(["--exact", PIPE_PROBE_TEST, "--format", "terse"])
            .env("ACP_PIPE_PROBE_ROLE", role)
            .env("ACP_PIPE_PROBE_ROOT", root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Ok(command)
    }

    async fn run_pipe_probe_role() -> Result<bool> {
        let Ok(role) = std::env::var("ACP_PIPE_PROBE_ROLE") else {
            return Ok(false);
        };
        let root = std::path::PathBuf::from(
            std::env::var_os("ACP_PIPE_PROBE_ROOT").context("pipe probe root missing")?,
        );
        match role.as_str() {
            "owner" => {
                // Use the same Rust test executable, not cmd/start redirection:
                // the descendant explicitly inherits both native pipe handles.
                let mut command = pipe_probe_command("descendant", &root)?;
                command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
                let mut descendant = command.spawn().context("start pipe owner")?;
                tokio::time::timeout(CLEANUP_DEADLINE, async {
                    while !root.join("ready").is_file() {
                        ensure!(
                            descendant.try_wait()?.is_none(),
                            "pipe owner exited before ready"
                        );
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Ok::<_, anyhow::Error>(())
                })
                .await
                .context("pipe owner readiness deadline")??;
                writeln!(std::io::stdout(), "pipe-owner-started")?;
                std::io::stdout().flush()?;
                // Deliberately bypass Drop: keep a real orphan with inherited pipes.
                std::process::exit(23);
            }
            "descendant" => {
                writeln!(std::io::stderr(), "pipe-owner-stderr")?;
                std::io::stderr().flush()?;
                std::fs::write(root.join("ready"), std::process::id().to_string())?;
                tokio::time::timeout(RESPONSE_DEADLINE, async {
                    while root.is_dir() && !root.join("release").is_file() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .context("pipe owner release deadline")?;
                Ok(true)
            }
            _ => anyhow::bail!("unknown pipe probe role: {role}"),
        }
    }

    struct PipeProbe {
        child: Option<FixtureChild>,
        stdout: BufReader<tokio::process::ChildStdout>,
        stderr: BufReader<tokio::process::ChildStderr>,
        root: TempDir,
        _manager: ChildProcessManager,
    }

    impl PipeProbe {
        async fn spawn() -> Result<Self> {
            let root = tempfile::tempdir()?;
            let manager = ChildProcessManager::new();
            let mut child =
                FixtureChild::spawn(&manager, pipe_probe_command("owner", root.path())?).await?;
            #[cfg(unix)]
            {
                // ChildProcessManager already made the owner its group leader.
                // Retain that group identity after leader exit for fallback cleanup.
                let core = CommandWrap::from(Command::new(std::env::current_exe()?));
                child.0 = ProcessGroup::leader().wrap_child(child.0, &core)?;
            }
            let stdout = BufReader::new(child.0.stdout().take().context("probe stdout")?);
            let stderr = BufReader::new(child.0.stderr().take().context("probe stderr")?);
            Ok(Self {
                child: Some(child),
                stdout,
                stderr,
                root,
                _manager: manager,
            })
        }

        async fn assert_orphan_holds_pipes(&mut self) -> Result<()> {
            let child = self.child.as_mut().context("probe child missing")?;
            let status = tokio::time::timeout(CLEANUP_DEADLINE, child.wait()).await??;
            ensure!(status.code() == Some(23), "probe owner status: {status}");
            tokio::time::timeout(CLEANUP_DEADLINE, async {
                loop {
                    let mut line = String::new();
                    ensure!(
                        self.stdout.read_line(&mut line).await? != 0,
                        "probe stdout EOF before marker"
                    );
                    if line.trim() == "pipe-owner-started" {
                        break;
                    }
                }
                let mut line = String::new();
                self.stderr.read_line(&mut line).await?;
                ensure!(
                    line.trim() == "pipe-owner-stderr",
                    "probe stderr marker: {line:?}"
                );
                Ok::<_, anyhow::Error>(())
            })
            .await
            .context("orphan probe marker deadline")??;
            for (name, stream) in [
                (
                    "stdout",
                    &mut self.stdout as &mut (dyn tokio::io::AsyncRead + Unpin),
                ),
                ("stderr", &mut self.stderr),
            ] {
                let mut tail = Vec::new();
                let read =
                    tokio::time::timeout(Duration::from_millis(50), stream.read_to_end(&mut tail))
                        .await;
                ensure!(
                    read.is_err(),
                    "descendant did not retain {name}: read={read:?}, tail={tail:?}"
                );
            }
            phase(format_args!(
                "orphan owner reaped; both pipes retained; descendant={}",
                std::fs::read_to_string(self.root.path().join("ready"))?
            ));
            Ok(())
        }

        async fn assert_eof(&mut self) -> Result<()> {
            tokio::time::timeout(CLEANUP_DEADLINE, async {
                self.stdout.read_to_end(&mut Vec::new()).await?;
                self.stderr.read_to_end(&mut Vec::new()).await?;
                Ok::<_, std::io::Error>(())
            })
            .await
            .context("orphan pipe EOF deadline")??;
            Ok(())
        }
    }

    impl Drop for PipeProbe {
        fn drop(&mut self) {
            // Errors/assertions must retire the orphan, not leave a pipe reader
            // blocking runtime shutdown. Windows' job also kills on handle drop.
            if let Some(child) = self.child.as_mut() {
                let _ = child.0.start_kill();
            }
        }
    }

    #[tokio::test]
    async fn orphan_probe_retains_inherited_pipes() -> Result<()> {
        harnx_core::require_nextest();
        if run_pipe_probe_role().await? {
            return Ok(());
        }
        let _body = TestBodyMarker;
        for terminate in [false, true] {
            let mut probe = PipeProbe::spawn().await?;
            probe.assert_orphan_holds_pipes().await?;
            if terminate {
                probe
                    .child
                    .as_mut()
                    .context("probe child missing")?
                    .0
                    .start_kill()
                    .context("terminate orphan tree")?;
            } else {
                std::fs::write(probe.root.path().join("release"), "release")?;
            }
            probe.assert_eof().await?;
        }
        Ok(())
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_job_closes_inherited_pipes_after_owner_exit() -> Result<()> {
        harnx_core::require_nextest();
        let _body = TestBodyMarker;
        for kill_explicitly in [true, false] {
            let mut probe = PipeProbe::spawn().await?;
            probe.assert_orphan_holds_pipes().await?;
            if kill_explicitly {
                // Must work after the leader has been reaped, unlike taskkill /T.
                probe
                    .child
                    .as_mut()
                    .context("probe child missing")?
                    .0
                    .start_kill()
                    .context("terminate orphan job")?;
            }
            // KillOnDrop is the fallback for protocol failure/unwind paths.
            drop(probe.child.take());
            probe.assert_eof().await?;
            phase(format_args!(
                "orphan job cleanup complete explicit={kill_explicitly}"
            ));
        }
        Ok(())
    }

    #[tokio::test]
    async fn stdout_contains_only_protocol_frames() -> Result<()> {
        let _body = TestBodyMarker;
        let responses = exchange(None).await?;
        assert!(
            responses[0].is_object(),
            "Response should be valid JSON object"
        );
        assert!(
            !responses[0].to_string().contains('\n'),
            "Response should not contain embedded newlines"
        );
        Ok(())
    }
}

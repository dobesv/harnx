use super::*;
use futures_util::StreamExt;

fn bash_path(path: &str) -> String {
    shell_words::join([path])
}

#[test]
fn bash_path_quote_preserves_backslashes_and_spaces() {
    let path = r"C:\Users\Runner Admin\AppData\Local\Temp\sample.txt";
    let quoted = shell_words::join([path]);
    assert_eq!(shell_words::split(&quoted).unwrap(), [path]);
}

/// `harnx call bash` against a real CLI process, real broker and the
/// built `harnx-fs-tools` fixture server (#2630 task 5).
#[tokio::test(flavor = "multi_thread")]
async fn call_bash_e2e_selection_purity_and_failures() -> anyhow::Result<()> {
    let Some(mut server) = harnx_test_bins::spawn_nats_server().await? else {
        eprintln!("skipping: nats-server not available");
        return Ok(());
    };
    let fixture = BashCliFixture::configure(server.url())?;
    fixture.start_worker().await?;
    let sample = fixture.dir.path().join("sample.txt");
    let sample = sample.to_string_lossy().into_owned();
    fixture.assert_agentless_inferred(&sample).await?;
    fixture.assert_shell_and_failures(&sample).await?;
    fixture.assert_named_selection(&sample).await?;
    drop(fixture);
    server.kill()?;
    Ok(())
}

struct BashCliFixture {
    dir: tempfile::TempDir,
    nats_url: String,
}

impl BashCliFixture {
    fn configure(nats_url: &str) -> anyhow::Result<Self> {
        let fs_tools = std::env::current_exe()?
            .parent()
            .and_then(|dir| dir.parent())
            .map(|dir| dir.join(format!("harnx-fs-tools{}", std::env::consts::EXE_SUFFIX)))
            .filter(|path| path.is_file())
            .ok_or_else(|| anyhow::anyhow!("build harnx-fs-tools before this test"))?;

        // Keep spaces in the fixture root so YAML paths and Bash arguments exercise quoting.
        let dir = tempfile::Builder::new()
            .prefix("harnx-bash fixture with spaces ")
            .tempdir()?;
        std::fs::create_dir_all(dir.path().join("nats_servers"))?;
        std::fs::create_dir_all(dir.path().join("agents"))?;
        std::fs::write(
            dir.path().join("nats_servers/local.yaml"),
            // The broker has no auth; the token is needed by managed tool servers
            // (same convention as the runtime it-harness fixtures).
            format!("url: {}\ntoken: cli-test\n", nats_url),
        )?;
        std::fs::create_dir_all(dir.path().join("tool_servers"))?;
        std::fs::write(
            dir.path().join("config.yaml"),
            "model: test:test-model\ntool_use: true\n",
        )?;
        // Tool servers load one yaml per server from the config dir
        // (`TOOL_SERVERS_DIR_NAME`), never inline in config.yaml.
        let fs_server_config = serde_json::json!({
            "command": fs_tools.to_string_lossy(),
            "args": [
                "--name",
                "fs",
                "--allow-read",
                dir.path().to_string_lossy(),
            ],
        });
        std::fs::write(
            dir.path().join("tool_servers/fs.yaml"),
            serde_yaml::to_string(&fs_server_config)?,
        )?;
        std::fs::write(
            dir.path().join("agents/limited.md"),
            // No `model:` line: the agent inherits the config's current model,
            // which skips the model-registry lookup (no provider compiled in).
            "---\nuse_tools: [fs_read]\n---\nReads only.\n",
        )?;
        std::fs::write(
            dir.path().join("agents/bare.md"),
            "---\nuse_tools: []\n---\nNo tools.\n",
        )?;
        std::fs::write(dir.path().join("sample.txt"), "reserved content\n")?;

        Ok(Self {
            dir,
            nats_url: nats_url.to_string(),
        })
    }

    async fn start_worker(&self) -> anyhow::Result<()> {
        // One shared server scope for the in-process worker and the CLI children
        // (nextest runs each test in its own process, so env mutation is safe).
        // The fs tool server spawned by the worker inherits this process env, so
        // the worker must see the same NATS endpoint and config dir as the CLI.
        let scope = harnx_core::instance::ServerScope::new();
        std::env::set_var("HARNX_SERVER_SCOPE", scope.as_str());
        std::env::set_var("HARNX_NATS_URL", &self.nats_url);
        std::env::set_var("HARNX_NATS_SERVER", "local");
        std::env::set_var("HARNX_NATS_TOKEN", "cli-test");
        std::env::set_var("HARNX_CONFIG_DIR", self.dir.path().as_os_str());
        std::env::set_var("HARNX_STATE_DIR", self.dir.path().as_os_str());

        // The CLI expects a cluster worker: run one in-process, managing the fs
        // tool server for reservation claims.
        let mut worker_config =
            harnx_runtime::config::Config::load_from_file(&self.dir.path().join("config.yaml"))?;
        worker_config.nats_routing = harnx_runtime::config::NatsRouting::Cluster("local".into());
        let worker_config =
            std::sync::Arc::new(harnx_runtime::config::ConfigLock::new(worker_config));
        let worker_client = async_nats::connect(&self.nats_url).await?;
        let mut worker_ready = worker_client
            .subscribe(harnx_runtime::nats_worker::worker_ready_subject("local"))
            .await?;
        worker_client.flush().await?;
        let daemon = harnx_runtime::nats_worker::WorkerDaemonConfig::managing(
            "local",
            "call-bash-e2e-worker",
        );
        tokio::spawn(harnx_runtime::nats_worker::run_worker_daemon(
            worker_config,
            daemon,
            None,
            None,
        ));
        tokio::time::timeout(std::time::Duration::from_secs(30), worker_ready.next())
            .await?
            .ok_or_else(|| anyhow::anyhow!("worker readiness channel closed"))?;

        Ok(())
    }

    async fn run(&self, args: &[&str]) -> (Option<i32>, std::process::Output) {
        let cli_args: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
        let mut command = cli_command();
        command
            .env("HARNX_CONFIG_DIR", self.dir.path())
            .env("HARNX_STATE_DIR", self.dir.path())
            .env("HARNX_NATS_URL", &self.nats_url)
            .env("HARNX_NATS_SERVER", "local")
            .args(cli_args);
        let output = command.output().unwrap();
        (output.status.code(), output)
    }

    async fn assert_agentless_inferred(&self, sample: &str) -> anyhow::Result<()> {
        // Agentless inferred: literal fs_read admitted through the reservation.
        let script = format!(
            "result=$(fs_read --path {}); echo \"got:$result\"",
            bash_path(sample)
        );
        let (code, output) = self.run(&["call", "bash", &script, "--json"]).await;
        assert_eq!(code, Some(0), "agentless inferred: {output:?}");
        let payload: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(payload["status"], "ok");
        assert_eq!(payload["trace"][0]["name"], "fs_read");
        assert!(
            payload["stdout"]
                .as_str()
                .unwrap_or_default()
                .contains("reserved content"),
            "{payload}"
        );

        Ok(())
    }

    async fn assert_shell_and_failures(&self, sample: &str) -> anyhow::Result<()> {
        // Pure shell needs no tools: the synthetic outer bash runs alone.
        let (code, output) = self
            .run(&["call", "bash", "echo hello world", "--json"])
            .await;
        assert_eq!(code, Some(0), "pure shell: {output:?}");
        let payload: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(payload["status"], "ok");
        assert!(payload["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("hello world"));
        // Single valid JSON on stdout, no progress contamination.
        assert!(payload.get("content").is_some(), "{payload}");

        // Unmatched explicit selectors never infer a fallback.
        let script = format!("fs_read --path {}", bash_path(sample));
        let (code, output) = self
            .run(&["call", "bash", &script, "--tools", "no_such_tool", "--json"])
            .await;
        assert_eq!(code, Some(1), "unmatched explicit must fail: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("error:"), "{stderr}");
        assert!(!String::from_utf8_lossy(&output.stdout).contains("reserved content"));

        // Nonzero script exit reports failure with the exit code in the result.
        let (code, output) = self
            .run(&["call", "bash", "echo before; exit 3", "--json"])
            .await;
        assert_eq!(code, Some(1), "nonzero exit must fail the CLI: {output:?}");
        let payload: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(payload["partial_result"]["exit_code"], 3, "{payload}");

        Ok(())
    }

    async fn assert_named_selection(&self, sample: &str) -> anyhow::Result<()> {
        let script = format!("fs_read --path {}", bash_path(sample));
        // Named inferred: admission is the agent's U ∪ B, not the inferred names.
        let (code, output) = self
            .run(&["-a", "limited", "call", "bash", &script, "--json"])
            .await;
        assert_eq!(code, Some(0), "named inferred: {output:?}");
        let payload: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(payload["trace"][0]["name"], "fs_read");

        // A named agent with an empty U admits nothing by inference alone.
        let (code, output) = self
            .run(&["-a", "bare", "call", "bash", &script, "--json"])
            .await;
        assert_eq!(code, Some(1), "named inference must not widen: {output:?}");
        assert!(!String::from_utf8_lossy(&output.stdout).contains("reserved content"));

        // Named explicit is additive: E adds fs_read on top of the empty U.
        let (code, output) = self
            .run(&[
                "-a", "bare", "call", "bash", &script, "--tools", "fs_read", "--json",
            ])
            .await;
        assert_eq!(code, Some(0), "named explicit additive: {output:?}");
        let payload: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        assert!(
            payload["stdout"]
                .as_str()
                .unwrap_or_default()
                .contains("reserved content"),
            "{payload}"
        );

        Ok(())
    }
}

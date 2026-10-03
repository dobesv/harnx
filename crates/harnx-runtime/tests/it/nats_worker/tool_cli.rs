use super::*;
use anyhow::{ensure, Context};
use futures_util::StreamExt;
use harnx_core::instance::ServerScope;
use harnx_runtime::config::{ConfigLock, GlobalConfig, NatsRouting, ToolServerConfig};
use harnx_toolset::{ToolInvocation, ToolInvokeError, ToolSpec, Toolset};
use serde_json::Value;
use std::{path::PathBuf, process::Output};
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};

fn binary(name: &str) -> Result<PathBuf> {
    let path = std::env::current_exe()?
        .parent()
        .context("test executable directory")?
        .parent()
        .context("workspace binary directory")?
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    ensure!(
        path.is_file(),
        "build workspace first: {} is missing",
        path.display()
    );
    Ok(path)
}

fn seed_cli_config(dir: &std::path::Path, url: &str) -> Result<()> {
    std::fs::create_dir_all(dir.join("nats_servers"))?;
    std::fs::create_dir_all(dir.join("agents"))?;
    std::fs::write(
        dir.join("nats_servers/local.yaml"),
        format!("url: {}\ntoken: cli-test\n", url),
    )?;
    // The broker has no authentication; the token is needed only by managed hooks.
    std::fs::write(
            dir.join("config.yaml"),
            "model: test:test-model\ntool_use: true\nuse_tools: [fixture_echo]\ntoolsets:\n  inspection: [fixture_echo, fixture_hidden]\n  packaged_read: [pkg__fs_read]\n",
        )?;
    std::fs::write(dir.join("agents/limited.md"), "---\nmodel: test:test-model\nuse_tools: [fixture_echo, fixture_delegate]\n---\nNo inference for direct calls.\n")?;
    std::fs::write(
        dir.join("agents/waiter.md"),
        "---\nmodel: test:test-model\nuse_tools: [fixture_wait]\n---\nWait tool only.\n",
    )?;
    Ok(())
}

fn configure_managed_tools(
    config: &mut Config,
    dir: &std::path::Path,
    package: bool,
) -> Result<()> {
    let fs = binary("harnx-fs-tools")?;
    config.tool_servers = vec![ToolServerConfig {
        name: "fs".into(),
        command: fs.to_string_lossy().into_owned(),
        args: vec![
            "--name".into(),
            "fs".into(),
            "--allow-read".into(),
            dir.to_string_lossy().into_owned(),
        ],
        env: Default::default(),
        enabled: true,
        description: None,
        package: package.then(|| "pkg".into()),
        hooks: None,
    }];
    let path = if package {
        dir.join("packages/pkg/agents/limited.md")
    } else {
        dir.join("agents/limited.md")
    };
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(
        path,
        "---\nmodel: /test:test-model\nuse_tools: [fs_read]\n---\nInspect files only.\n",
    )?;
    std::fs::write(
        dir.join("file with spaces.txt"),
        "complete CLI file output\n",
    )?;
    Ok(())
}

fn nested_cli_inference(calls: &Arc<AtomicUsize>) -> harnx_runtime::agent_loop::AgentCallFn {
    let model_calls = calls.clone();
    Arc::new(move |_, _, _| {
        model_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Ok((
                String::new(),
                None,
                vec![ToolCall::new(
                    "fixture_echo".into(),
                    json!({"text":"nested Ask"}),
                    None,
                    None,
                )],
                Default::default(),
            ))
        })
    })
}

#[path = "tool_cli/hook_fixture.rs"]
mod hook_fixture;
use hook_fixture::CliHookFixture;

struct Harness {
    dir: tempfile::TempDir,
    client: async_nats::Client,
    config: GlobalConfig,
    scope: ServerScope,
    worker: AbortOnDropHandle<Result<()>>,
    calls: Arc<AtomicUsize>,
    _env: Vec<EnvGuard>,
    _broker: common::NatsServerHandle,
}
impl Harness {
    async fn start(managed: bool, package: bool) -> Result<Option<Self>> {
        let Some(broker) = require_nats_server().await? else {
            return Ok(None);
        };
        let dir = tempfile::tempdir()?;
        seed_cli_config(dir.path(), broker.url())?;
        let scope = ServerScope::new();
        let env = vec![
            EnvGuard::set("HARNX_CONFIG_DIR", dir.path().to_str().unwrap()),
            EnvGuard::set("HARNX_STATE_DIR", dir.path().to_str().unwrap()),
            EnvGuard::set("HARNX_NATS_URL", broker.url()),
            EnvGuard::set("HARNX_NATS_TOKEN", "cli-test"),
            EnvGuard::set("HARNX_NATS_SERVER", "local"),
            EnvGuard::set("HARNX_SERVER_SCOPE", scope.as_str()),
        ];
        let mut config = Config::load_from_file(&dir.path().join("config.yaml"))?;
        config.nats_routing = NatsRouting::Default;
        config.model = harnx_core::model::Model::new("test", "test-model");
        if managed {
            configure_managed_tools(&mut config, dir.path(), package)?;
        }
        let client = async_nats::connect(broker.url()).await?;
        let global = Arc::new(ConfigLock::new(config));
        let calls = Arc::new(AtomicUsize::new(0));
        let call_fn = nested_cli_inference(&calls);
        let mut ready = client
            .subscribe(harnx_runtime::nats_worker::worker_ready_subject("local"))
            .await?;
        client.flush().await?;
        let daemon = if managed {
            WorkerDaemonConfig::managing("local", "cli-worker")
        } else {
            WorkerDaemonConfig::new("local", "cli-worker")
        }
        .with_tool_reservation_timing_for_test(Duration::from_secs(10), Duration::ZERO);
        let mut worker = AbortOnDropHandle::new(tokio::spawn(run_worker_daemon(
            global.clone(),
            daemon,
            Some(call_fn),
            None,
        )));
        tokio::select! {
            response = tokio::time::timeout(CI_SAFE_TIMEOUT, ready.next()) => { response?.context("worker readiness closed")?; },
            result = &mut worker => anyhow::bail!("worker failed during startup: {result:?}"),
        }
        Ok(Some(Self {
            dir,
            client,
            config: global,
            scope,
            worker,
            calls,
            _env: env,
            _broker: broker,
        }))
    }
    fn cli_process(&self, args: &[&str]) -> Result<tokio::process::Command> {
        let mut command = tokio::process::Command::new(binary("harnx")?);
        command
            .args(args)
            .env("HARNX_CONFIG_DIR", self.dir.path())
            .env("HARNX_STATE_DIR", self.dir.path())
            // Explicit agent@cluster must win over an unrelated frontend default.
            .env(
                "HARNX_NATS_SERVER",
                if args.contains(&"--agent") {
                    "unselected"
                } else {
                    "local"
                },
            )
            .env_remove("HARNX_SERVER_SCOPE")
            .kill_on_drop(true);
        // Exercise Windows' main-stack budget on Linux too, including real
        // reservation discovery, hooks and named-session execution.
        #[cfg(target_os = "linux")]
        unsafe {
            command.pre_exec(|| {
                let limit = libc::rlimit {
                    rlim_cur: 1024 * 1024,
                    rlim_max: 1024 * 1024,
                };
                if libc::setrlimit(libc::RLIMIT_STACK, &limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok(command)
    }
    async fn run(&self, args: &[&str]) -> Result<Output> {
        let mut command = self.cli_process(args)?;
        let mut observed = self.client.subscribe(">".to_owned()).await?;
        self.client.flush().await?;
        let output = tokio::time::timeout(Duration::from_secs(90), command.output())
            .await
            .context("CLI did not exit")?
            .context("run CLI")?;
        let fence = format!("cli.test.fence.{}", uuid::Uuid::new_v4());
        self.client.publish(fence.clone(), "".into()).await?;
        self.client.flush().await?;
        let mut reserved = Vec::new();
        let mut released = std::collections::HashSet::new();
        tokio::time::timeout(CI_SAFE_TIMEOUT, async {
            while let Some(message) = observed.next().await {
                if message.subject.as_str() == fence {
                    break;
                }
                if let Ok(value) = serde_json::from_slice::<
                    harnx_runtime::nats_worker::tool_reservation::Reserved,
                >(&message.payload)
                {
                    reserved.push((value.control_subject, value.reservation_id));
                }
                if let Ok(
                    harnx_runtime::nats_worker::tool_reservation::ToolReservationControl::Release(
                        value,
                    ),
                ) = serde_json::from_slice(&message.payload)
                {
                    released.insert((message.subject.to_string(), value.reservation_id));
                }
            }
        })
        .await
        .context("reservation observer did not reach fence")?;
        if !args.contains(&"--agent") {
            assert_eq!(
                reserved.len(),
                1,
                "CLI did not use virtual reservation: {args:?}; status={}; stdout={}; stderr={}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        for reservation in reserved {
            assert!(
                released.contains(&reservation),
                "CLI exited without releasing {reservation:?}: {args:?}"
            );
        }
        Ok(output)
    }
    async fn json(&self, args: &[&str], success: bool) -> Result<Value> {
        let output = self.run(args).await?;
        assert_eq!(
            output.status.success(),
            success,
            "args={args:?} stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if !success {
            assert!(String::from_utf8_lossy(&output.stderr).contains("error:"));
        }
        serde_json::from_slice(&output.stdout).with_context(|| {
            format!(
                "non-JSON stdout: {}",
                String::from_utf8_lossy(&output.stdout)
            )
        })
    }
    async fn empty_fs_registration(&self) -> Result<()> {
        let js = async_nats::jetstream::new(self.client.clone());
        let registry = js
            .get_key_value(harnx_toolset_server::TOOL_REGISTRY_BUCKET)
            .await?;
        let identity = harnx_toolset::server_identity_token(Some("pkg"), "fs", "fs");
        poll_until(async || {
            let mut keys = registry.keys().await?;
            while let Some(key) = keys.next().await {
                let key = key?;
                if key.ends_with(&format!(".{identity}")) && registry.get(&key).await?.is_some() {
                    return Ok(false);
                }
            }
            Ok(true)
        })
        .await
    }
}

impl Harness {
    async fn assert_packaged_catalog(&self) -> Result<()> {
        let all = self.json(&["list", "tools", "--json"], true).await?;
        assert!(all.as_array().unwrap().iter().all(|tool| tool["name"]
            != harnx_runtime::session_history::TOOL_NAME
            && !tool["name"].as_str().unwrap().ends_with("_session_handoff")));
        assert!(all
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "pkg__fs_read"));
        assert!(all
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "pkg__fs_write"));
        self.empty_fs_registration().await?;
        let list = self
            .json(&["list", "tools", "pkg__fs_r*", "--json"], true)
            .await?;
        assert!(!list.as_array().unwrap().is_empty());
        assert!(list
            .as_array()
            .unwrap()
            .iter()
            .all(|tool| tool["name"].as_str().unwrap().starts_with("pkg__fs_r")));
        self.empty_fs_registration().await?;
        let group = self
            .json(&["list", "tools", "packaged_read", "--json"], true)
            .await?;
        assert_eq!(group.as_array().unwrap().len(), 1);
        assert_eq!(group[0]["name"], "pkg__fs_read");
        self.empty_fs_registration().await?;
        let empty = self
            .json(&["list", "tools", "not-present*", "--json"], true)
            .await?;
        assert_eq!(empty, json!([]));
        Ok(())
    }

    async fn assert_packaged_schema(&self) -> Result<()> {
        let selected = self
            .json(
                &["--agent", "pkg/limited@local", "list", "tools", "--json"],
                true,
            )
            .await?;
        assert_eq!(selected.as_array().unwrap().len(), 1);
        assert_eq!(selected[0]["name"], "fs_read");
        self.empty_fs_registration().await?;
        let info = self
            .json(&["info", "tool", "pkg__fs_read", "--json"], true)
            .await?;
        assert_eq!(info["name"], "pkg__fs_read");
        assert!(info["parameters"]["properties"]["path"].is_object());
        for field in [
            "mcp_tool_name",
            "mcp_server_name",
            "call_template",
            "result_template",
            "kind",
            "read_only_hint",
            "idempotent_hint",
        ] {
            assert!(info.get(field).is_some(), "missing {field}");
        }
        self.empty_fs_registration().await?;
        Ok(())
    }

    async fn assert_packaged_calls(&self) -> Result<String> {
        let args = json!({"path":self.dir.path().join("file with spaces.txt")}).to_string();
        let output = self
            .json(&["call", "tool", "pkg__fs_read", &args, "--json"], true)
            .await?;
        assert!(output.to_string().contains("complete CLI file output"));
        self.empty_fs_registration().await?;
        let named = self
            .json(
                &[
                    "call",
                    "tool",
                    "fs_read",
                    &args,
                    "--agent",
                    "pkg/limited@local",
                    "--json",
                ],
                true,
            )
            .await?;
        assert!(named.to_string().contains("complete CLI file output"));
        self.empty_fs_registration().await?;
        Ok(args)
    }

    async fn assert_packaged_failures(&self) -> Result<()> {
        for name in ["fs_write", "does_not_exist"] {
            let error = self
                .json(
                    &[
                        "--agent",
                        "pkg/limited@local",
                        "info",
                        "tool",
                        name,
                        "--json",
                    ],
                    false,
                )
                .await?;
            assert!(error["error"].as_str().unwrap().contains("not available"));
            self.empty_fs_registration().await?;
        }
        let invalid = self
            .json(
                &["call", "tool", "pkg__fs_read", r#"{"path":5}"#, "--json"],
                false,
            )
            .await?;
        assert!(invalid.to_string().contains("error") || invalid.to_string().contains("isError"));
        self.empty_fs_registration().await?;
        let human = self.run(&["info", "tool", "pkg__fs_read"]).await?;
        assert!(human.status.success());
        assert!(String::from_utf8_lossy(&human.stdout).contains("Input schema:"));
        self.empty_fs_registration().await?;
        let missing = self
            .json(&["call", "tool", "does_not_exist", "{}", "--json"], false)
            .await?;
        assert!(missing["error"].as_str().unwrap().contains("not available"));
        self.empty_fs_registration().await?;
        Ok(())
    }

    async fn assert_managed_hook_policy(&self, args: &str) -> Result<()> {
        self.set_managed_agent_policy("ask")?;
        let transformed = self
            .json(
                &[
                    "--agent",
                    "pkg/limited@local",
                    "call",
                    "tool",
                    "fs_read",
                    "{}",
                    "--json",
                ],
                true,
            )
            .await?;
        assert!(transformed.to_string().contains("complete CLI file output"));
        self.empty_fs_registration().await?;
        self.empty_hook_registrations().await?;
        self.set_managed_agent_policy("deny")?;
        let denied = self
            .json(
                &[
                    "--agent",
                    "pkg/limited@local",
                    "call",
                    "tool",
                    "fs_read",
                    args,
                    "--json",
                ],
                false,
            )
            .await?;
        assert_eq!(denied["blocked_by_hook"], true);
        assert!(denied["error"]
            .as_str()
            .unwrap()
            .contains("CLI managed policy"));
        self.empty_fs_registration().await?;
        self.empty_hook_registrations().await?;
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cli_managed_activation_package_allowlist_patterns_and_release_all_outcomes() -> Result<()>
{
    let Some(h) = Harness::start(true, true).await? else {
        return Ok(());
    };
    h.assert_packaged_catalog().await?;
    h.assert_packaged_schema().await?;
    let args = h.assert_packaged_calls().await?;
    h.assert_packaged_failures().await?;
    h.assert_managed_hook_policy(&args).await?;
    assert_eq!(
        h.calls.load(Ordering::SeqCst),
        0,
        "operator ran root inference"
    );
    assert!(!h.worker.is_finished());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cli_real_hooks_results_validation_timeouts_nested_asks_and_identity() -> Result<()> {
    let Some(h) = Harness::start(false, false).await? else {
        return Ok(());
    };
    let fixture = CliHookFixture::start(&h).await?;
    fixture.assert_inventory().await?;
    fixture.assert_transformed_results().await?;
    fixture.assert_deny_validation().await?;
    fixture.assert_error_partial_transport().await?;
    fixture.assert_timeout_cleanup().await?;
    fixture.assert_nested_consent().await?;
    fixture.stop();
    Ok(())
}

struct CollisionTools {
    server: &'static str,
    raw: &'static str,
    calls: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl Toolset for CollisionTools {
    fn name(&self) -> &str {
        self.server
    }
    fn tools(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: self.raw.into(),
            description: "collision fixture".into(),
            input_schema: json!({"type":"object"}),
            cancellation_guarantee: Default::default(),
            idempotent_hint: false,
            read_only_hint: false,
            timeout_secs: None,
            meta: None,
        }]
    }
    async fn invoke(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
    ) -> std::result::Result<Value, ToolInvokeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"must_not_run":true}))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cli_ambiguous_names_fail_without_dispatch_and_closed_generation_is_rejected() -> Result<()>
{
    let Some(h) = Harness::start(false, false).await? else {
        return Ok(());
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let shutdown = CancellationToken::new();
    let mut servers = Vec::new();
    for (server, raw) in [("a", "b_c"), ("a_b", "c")] {
        servers.push(AbortOnDropHandle::new(tokio::spawn(
            harnx_toolset_server::serve_with_shutdown(
                Arc::new(CollisionTools {
                    server,
                    raw,
                    calls: calls.clone(),
                }),
                h.scope.clone(),
                harnx_nats_common::connect::NatsConnection {
                    client: h.client.clone(),
                    replicas: 1,
                },
                harnx_toolset_server::ServeLifecycle::new(shutdown.clone(), None),
            ),
        )));
    }
    let js = async_nats::jetstream::new(h.client.clone());
    poll_until(async || {
        let Ok(registry) = js
            .get_key_value(harnx_toolset_server::TOOL_REGISTRY_BUCKET)
            .await
        else {
            return Ok(false);
        };
        for server in ["a", "a_b"] {
            let key = harnx_toolset_server::registration_key(
                &h.scope,
                &harnx_toolset::server_identity_token(None, "", server),
            );
            if registry.get(key).await?.is_none() {
                return Ok(false);
            }
        }
        Ok(true)
    })
    .await?;
    for args in [
        vec!["info", "tool", "a_b_c", "--json"],
        vec!["call", "tool", "a_b_c", "{}", "--json"],
        vec!["list", "tools", "a_*", "--json"],
    ] {
        let error = h.json(&args, false).await?;
        assert!(
            error["error"].as_str().unwrap().contains("ambiguous"),
            "{error}"
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_closed_reservation(&h).await?;
    shutdown.cancel();
    Ok(())
}

impl Harness {
    fn set_managed_agent_policy(&self, decision: &str) -> Result<()> {
        let expression = json!({
            "hookSpecificOutput":{"permissionDecision":decision,"permissionDecisionReason":"CLI managed policy"},
            "mutatedToolInput":{"path": self.dir.path().join("file with spaces.txt")}
        }).to_string();
        let hook = shell_words::join([
            binary("harnx-claude-compatible-hook-server")?
                .to_string_lossy()
                .into_owned(),
            "--event".into(),
            "PreToolUse".into(),
            "--jaq".into(),
            expression,
        ]);
        let header = json!({"model":"/test:test-model","use_tools":["fs_read"],"hooks":{"entries":[{"command":hook}]}});
        std::fs::write(
            self.dir.path().join("packages/pkg/agents/limited.md"),
            format!(
                "---\n{}---\nInspect files only.\n",
                serde_yaml::to_string(&header)?
            ),
        )?;
        Ok(())
    }
    async fn empty_hook_registrations(&self) -> Result<()> {
        let store = async_nats::jetstream::new(self.client.clone())
            .get_key_value(harnx_hookset_server::HOOK_REGISTRY_BUCKET)
            .await?;
        poll_until(async || {
            let mut keys = store.keys().await?;
            while let Some(key) = keys.next().await {
                if store.get(key?).await?.is_some() {
                    return Ok(false);
                }
            }
            Ok(true)
        })
        .await
    }
}

async fn assert_closed_reservation(h: &Harness) -> Result<()> {
    let mut config = h.config.read().clone();
    config.nats_routing = NatsRouting::Cluster("local".into());
    let view = harnx_runtime::nats_worker::tool_reservation::ToolReservationView {
        package: None,
        use_tools: vec!["a_b_c".into()],
    };
    let mut reservation = harnx_runtime::tool_reservation_client::ToolReservationHandle::open(
        config,
        harnx_runtime::SessionActivationRoute::ClusterShared,
        view.clone(),
    )
    .await?;
    let admitted = reservation.state();
    reservation.close().await?;
    assert!(harnx_runtime::operator_tools::cli::ensure_current(&reservation, &admitted).is_err());
    let error = harnx_runtime::operator_tools::cli::run_reserved_tool_command(
        &reservation,
        &view,
        harnx_runtime::operator_tools::cli::ToolCommandExecution {
            command: &harnx_runtime::operator_tools::OperatorToolCommand::Info {
                name: "a_b_c".into(),
            },
            json: true,
            abort: &create_abort_signal(),
        },
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("unavailable"));
    Ok(())
}

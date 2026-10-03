use super::*;
use crate::config::NatsRouting;
use crate::nats_worker::tests::{
    env_lock, seed_remote_config, spawn_test_nats, subagent_test_env, TestEnvGuard,
};
use crate::operator_tools::{request_command, OperatorToolCommand};
use crate::{NatsSession, NatsSessionConfig, SessionActivationRoute, SessionInitializer};
use harnx_core::instance::ServerScope;
use harnx_hooks::{HookEvent, HookOutcome, HookResult, HookResultControl};
use harnx_hookset::{FailPolicy, Hook, HookSpec};
use harnx_toolset::{ToolInvocation, ToolInvokeError, ToolSpec, Toolset};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex,
};
use tokio_util::sync::CancellationToken;

struct FixtureTools {
    seen: Arc<Mutex<Vec<Value>>>,
    started: Arc<tokio::sync::Notify>,
    cancelled: Arc<tokio::sync::Notify>,
    client: async_nats::Client,
}
#[async_trait::async_trait]
impl Toolset for FixtureTools {
    fn name(&self) -> &str {
        "fixture"
    }
    fn tools(&self) -> Vec<ToolSpec> {
        ["echo", "hidden", "wait", "delegate"].into_iter().map(|name| ToolSpec {
            name: name.into(), description: name.into(),
            input_schema: json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}),
            cancellation_guarantee: Default::default(), idempotent_hint: false, read_only_hint: false,
            timeout_secs: Some(0), meta: None,
        }).collect()
    }
    async fn invoke(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
    ) -> std::result::Result<Value, ToolInvokeError> {
        unreachable!()
    }
    async fn invoke_with_context(
        &self,
        invocation: ToolInvocation,
    ) -> std::result::Result<Value, ToolInvokeError> {
        if !invocation.args.get("text").is_some_and(Value::is_string) {
            return Err(ToolInvokeError::Recoverable("text must be a string".into()));
        }
        if invocation.tool == "delegate" {
            return self.delegate(invocation).await;
        }
        self.seen.lock().unwrap().push(invocation.args.clone());
        if invocation.tool == "wait" {
            self.started.notify_one();
            invocation.cancel.cancelled().await;
            self.cancelled.notify_one();
            return Err(ToolInvokeError::Recoverable("cancelled fixture".into()));
        }
        Ok(
            json!({"content":[{"type":"text","text":invocation.args["text"]}, {"type":"image","data":"aW1hZ2U=","mimeType":"image/png"}],
            "structuredContent":{"session":invocation.context.invoking_session_id,"agent":invocation.context.invoking_session.as_ref().and_then(|session| session.agent.clone())}, "extension":true}),
        )
    }
}

impl FixtureTools {
    async fn delegate(
        &self,
        invocation: ToolInvocation,
    ) -> std::result::Result<Value, ToolInvokeError> {
        let nested = self.nested_session(invocation).await?;
        nested
            .enqueue_text("nested tool request")
            .await
            .map_err(|error| ToolInvokeError::Recoverable(error.to_string()))?;
        let log = crate::nats_session_log::NatsSessionLog::new(
            nested.jetstream().clone(),
            nested.storage_key(),
        );
        let waited = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let entries = log.load_events_async().await?;
                if entries.iter().any(|(_, entry)| {
                    matches!(
                        entry,
                        harnx_core::session::SessionLogEntry::HitlApprovalRequested { .. }
                    )
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .map_err(|error| ToolInvokeError::Recoverable(error.to_string()))
        .and_then(|result| result.map_err(|error| ToolInvokeError::Recoverable(error.to_string())));
        if let Err(error) = waited {
            let entries = log
                .load_events_async()
                .await
                .map_err(|error| ToolInvokeError::Recoverable(error.to_string()))?;
            return Err(ToolInvokeError::Recoverable(format!(
                "{error}; nested log: {entries:?}"
            )));
        }
        Ok(
            json!({"resultType":"partial", "nested_session":nested.session_id(), "reason":"nested Ask awaits HITL"}),
        )
    }
    async fn nested_session(
        &self,
        invocation: ToolInvocation,
    ) -> std::result::Result<NatsSession, ToolInvokeError> {
        let nested = NatsSession::new(
            NatsSessionConfig {
                cluster: "local".into(),
                initializer: SessionInitializer::named("metis", Default::default()),
                session_id: Some("nested-operator".into()),
                activation_route: SessionActivationRoute::ClusterShared,
            },
            self.client.clone(),
            async_nats::jetstream::new(self.client.clone()),
            crate::utils::create_abort_signal(),
        )
        .await
        .map_err(|error| ToolInvokeError::Recoverable(error.to_string()))?;
        let lineage = invocation
            .context
            .run_context
            .as_ref()
            .expect("operator CALL lineage");
        let parent = serde_json::from_value(lineage.snapshot.clone()).unwrap();
        let admitted_at =
            chrono::DateTime::from_timestamp_millis(lineage.started_at_ms.try_into().unwrap())
                .unwrap();
        Ok(nested.with_inherited_admission(
            parent,
            invocation.context.call_id,
            admitted_at,
            crate::nats_session_metadata::InvocationEdgeKind::Delegation,
            None,
        ))
    }
}

struct Policy {
    calls: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl Hook for Policy {
    fn name(&self) -> &str {
        "operator-policy"
    }
    fn hooks(&self) -> Vec<HookSpec> {
        vec![HookSpec {
            event: "PreToolUse".into(),
            matcher: Some("^fixture_".into()),
            priority: 0,
            timeout_secs: None,
            fail_policy: FailPolicy::Closed,
        }]
    }
    async fn handle_hook(&self, payload: harnx_core::hooks::HookPayload) -> HookOutcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match payload.hook_event {
            HookEvent::PreToolUse { tool_input, .. }
                if tool_input.get("text") == Some(&json!("deny")) =>
            {
                HookOutcome {
                    control: HookResultControl::Block {
                        reason: "policy denied".into(),
                    },
                    result: Default::default(),
                }
            }
            HookEvent::PreToolUse { tool_input, .. }
                if tool_input.get("text") == Some(&json!("mutate")) =>
            {
                HookOutcome {
                    control: HookResultControl::Ask { reason: None },
                    result: HookResult {
                        mutated_tool_input: Some(json!({"text":"mutated with spaces"})),
                        ..Default::default()
                    },
                }
            }
            _ => HookOutcome {
                control: HookResultControl::Ask {
                    reason: Some("explicit Ask".into()),
                },
                result: Default::default(),
            },
        }
    }
}

async fn call(
    client: &async_nats::Client,
    request: &OperatorToolRequest,
    command: OperatorToolCommand,
) -> OperatorToolReply {
    let mut request = request.clone();
    request.command = command;
    request_command(
        client,
        "cluster.local.operator_tools",
        &request,
        &crate::utils::create_abort_signal(),
    )
    .await
    .unwrap()
}

struct LiveOperatorFixture {
    client: async_nats::Client,
    scope: ServerScope,
    tools: Arc<FixtureTools>,
    global: crate::config::GlobalConfig,
    readiness: harnx_healthz::Readiness,
    hooks_seen: Arc<AtomicUsize>,
    model_calls: Arc<AtomicUsize>,
    shutdown: CancellationToken,
    tool_server: AbortOnDropHandle<Result<()>>,
    hook_server: AbortOnDropHandle<Result<()>>,
    worker: AbortOnDropHandle<Result<()>>,
    session: NatsSession,
    request: OperatorToolRequest,
}

impl LiveOperatorFixture {
    async fn start(
        config: crate::config::Config,
        client: async_nats::Client,
        scope: ServerScope,
    ) -> Result<Self> {
        let tools = Arc::new(FixtureTools {
            seen: Arc::new(Mutex::new(Vec::new())),
            started: Arc::new(tokio::sync::Notify::new()),
            cancelled: Arc::new(tokio::sync::Notify::new()),
            client: client.clone(),
        });
        let shutdown = CancellationToken::new();
        let tool_server = spawn_operator_tools(tools.clone(), scope.clone(), shutdown.clone());
        let hooks_seen = Arc::new(AtomicUsize::new(0));
        let hook_server = spawn_operator_policy(&client, &scope, &shutdown, hooks_seen.clone());
        let global = operator_config(config);
        let readiness = harnx_healthz::Readiness::default();
        let model_calls = Arc::new(AtomicUsize::new(0));
        let worker = spawn_operator_worker(&global, &readiness, &model_calls);
        let session = operator_session(&client, "active-operator").await?;
        let request = OperatorToolRequest {
            version: 1,
            session_key: session.storage_key().into(),
            use_tools: None,
            tool_use: true,
            command: OperatorToolCommand::List { pattern: None },
            json: true,
        };
        let fixture = Self {
            client,
            scope,
            tools,
            global,
            readiness,
            hooks_seen,
            model_calls,
            shutdown,
            tool_server,
            hook_server,
            worker,
            session,
            request,
        };
        fixture.log().last_entry_async().await?;
        Ok(fixture)
    }

    fn log(&self) -> crate::nats_session_log::NatsSessionLog {
        crate::nats_session_log::NatsSessionLog::new(
            self.session.jetstream().clone(),
            self.session.storage_key(),
        )
    }

    async fn wait_ready(&self) -> Result<()> {
        let Self {
            client,
            scope,
            global,
            readiness,
            ..
        } = self;
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let snapshot = global.read().clone();
                let provider = crate::nats_tool_provider::NatsToolProvider::discover(
                    &snapshot,
                    scope.clone(),
                    crate::nats_tool_provider::NatsInFlightCalls::for_instance(scope),
                    None,
                )
                .await?;
                let hook = crate::nats_hook_provider::NatsHookProvider::discover_with_client(
                    client.clone(),
                    scope.clone(),
                )
                .await?;
                let tools_ready = provider
                    .declarations()
                    .iter()
                    .filter(|tool| tool.name.starts_with("fixture_"))
                    .count()
                    == 4;
                let hooks_ready = !hook.hooks().is_empty();
                if !readiness.is_ready() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    continue;
                }
                if tools_ready && hooks_ready {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        Ok(())
    }

    async fn assert_inventory(&self) -> Result<()> {
        let Self {
            client, request, ..
        } = self;
        let listing = call(client, request, OperatorToolCommand::List { pattern: None }).await;
        assert!(listing.error.is_none(), "{listing:?}");
        let tools: Vec<Value> = serde_json::from_str(&listing.output)?;
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["fixture_delegate", "fixture_echo", "fixture_wait"]
        );
        let hidden = call(
            client,
            request,
            OperatorToolCommand::Info {
                name: "fixture_hidden".into(),
            },
        )
        .await;
        assert!(hidden.error.unwrap().contains("not available"));
        Ok(())
    }

    async fn assert_active_handler(&self, mut local_config: crate::config::Config) -> Result<()> {
        let session = &self.session;
        local_config.nats_routing = NatsRouting::Cluster("local".into());
        local_config.remote_agent = Some(("metis".into(), "local".into()));
        local_config.session = Some(harnx_core::session::Session {
            id: "active-operator".into(),
            session_id: Some("active-operator".into()),
            ..Default::default()
        });
        let local = Arc::new(crate::config::ConfigLock::new(local_config));
        let mut output = Vec::new();
        crate::commands::run_command_with_output(
            &local,
            crate::utils::create_abort_signal(),
            ".call tool fixture_echo {\"text\": \"a \\\"quoted\\\" value with spaces\"}",
            &mut output,
        )
        .await?;
        let result: Value = serde_json::from_slice(&output)?;
        assert_eq!(
            result["content"][0]["text"],
            "a \"quoted\" value with spaces"
        );
        assert_eq!(result["content"][1]["data"], "aW1hZ2U=");
        assert_eq!(
            result["structuredContent"]["session"],
            session.storage_key()
        );
        assert_eq!(result["structuredContent"]["agent"], "metis");
        Ok(())
    }

    async fn assert_root_hook_policy(&self) -> Result<()> {
        let Self {
            client, request, ..
        } = self;
        let seen = &self.tools.seen;
        let mutated = call(
            client,
            request,
            OperatorToolCommand::Call {
                name: "fixture_echo".into(),
                args_json: r#"{"text":"mutate"}"#.into(),
            },
        )
        .await;
        assert!(mutated.error.is_none(), "{mutated:?}");
        assert!(mutated.output.contains("mutated with spaces"));
        let count = seen.lock().unwrap().len();
        let denied = call(
            client,
            request,
            OperatorToolCommand::Call {
                name: "fixture_echo".into(),
                args_json: r#"{"text":"deny"}"#.into(),
            },
        )
        .await;
        assert_eq!(denied.error.as_deref(), Some("policy denied"));
        assert_eq!(seen.lock().unwrap().len(), count);
        let invalid = call(
            client,
            request,
            OperatorToolCommand::Call {
                name: "fixture_echo".into(),
                args_json: r#"{"text":123}"#.into(),
            },
        )
        .await;
        assert!(invalid.error.is_some());
        assert_eq!(seen.lock().unwrap().len(), count);
        Ok(())
    }

    async fn assert_cancellation(&self) -> Result<()> {
        let Self {
            client,
            request,
            global,
            hooks_seen,
            model_calls,
            ..
        } = self;
        let FixtureTools {
            started, cancelled, ..
        } = self.tools.as_ref();
        let abort = crate::utils::create_abort_signal();
        let mut waiting = request.clone();
        waiting.command = OperatorToolCommand::Call {
            name: "fixture_wait".into(),
            args_json: r#"{"text":"wait"}"#.into(),
        };
        let waiting_task = tokio::spawn({
            let client = client.clone();
            let abort = abort.clone();
            async move {
                request_command(&client, "cluster.local.operator_tools", &waiting, &abort).await
            }
        });
        tokio::time::timeout(Duration::from_secs(30), started.notified()).await?;
        abort.set_ctrlc();
        assert!(waiting_task
            .await?
            .unwrap_err()
            .to_string()
            .contains("cancelled"));
        tokio::time::timeout(Duration::from_secs(30), cancelled.notified()).await?;
        assert!(hooks_seen.load(Ordering::SeqCst) >= 5);
        assert_eq!(model_calls.load(Ordering::SeqCst), 0);
        assert!(global.read().tui_confirm_tool_use.is_none());
        Ok(())
    }

    async fn assert_nested_consent(&self) -> Result<()> {
        let Self {
            client,
            request,
            global,
            model_calls,
            ..
        } = self;
        let seen = &self.tools.seen;
        let before_nested = seen.lock().unwrap().len();
        let nested = call(
            client,
            request,
            OperatorToolCommand::Call {
                name: "fixture_delegate".into(),
                args_json: r#"{"text":"delegate"}"#.into(),
            },
        )
        .await;
        assert!(
            nested
                .error
                .as_deref()
                .is_some_and(|error| error.contains("partial")),
            "{nested:?}"
        );
        assert!(nested.output.contains("nested Ask awaits HITL"));
        assert!(model_calls.load(Ordering::SeqCst) > 0);
        assert_eq!(
            seen.lock().unwrap().len(),
            before_nested,
            "root approval must not dispatch the nested tool"
        );
        let child_key = harnx_core::session_identity::session_key(Some("metis"), "nested-operator");
        let child_log = crate::nats_session_log::NatsSessionLog::new(
            self.session.jetstream().clone(),
            child_key,
        );
        let entries = child_log.load_events_async().await?;
        assert!(entries.iter().any(|(_, entry)| matches!(
            entry,
            harnx_core::session::SessionLogEntry::HitlApprovalRequested { .. }
        )));
        assert!(!entries.iter().any(|(_, entry)| matches!(
            entry,
            harnx_core::session::SessionLogEntry::HitlApprovalDecision { .. }
        )));
        assert!(global.read().tui_confirm_tool_use.is_none());
        Ok(())
    }

    async fn close(self) -> Result<()> {
        self.shutdown.cancel();
        self.tool_server.await??;
        self.hook_server.await??;
        drop(self.worker);
        Ok(())
    }
}

fn operator_config(mut config: crate::config::Config) -> crate::config::GlobalConfig {
    // Workers use the explicitly injected test broker, not frontend cluster routing.
    config.nats_routing = NatsRouting::Default;
    config.agent = None;
    config.model = harnx_core::model::Model::new("test", "test-model");
    Arc::new(crate::config::ConfigLock::new(config))
}

fn spawn_operator_tools(
    tools: Arc<FixtureTools>,
    scope: ServerScope,
    shutdown: CancellationToken,
) -> AbortOnDropHandle<Result<()>> {
    let connection = harnx_nats_common::connect::NatsConnection {
        client: tools.client.clone(),
        replicas: 1,
    };
    AbortOnDropHandle::new(tokio::spawn(harnx_toolset_server::serve_with_shutdown(
        tools,
        scope,
        connection,
        harnx_toolset_server::ServeLifecycle::new(shutdown, None),
    )))
}

fn spawn_operator_policy(
    client: &async_nats::Client,
    scope: &ServerScope,
    shutdown: &CancellationToken,
    calls: Arc<AtomicUsize>,
) -> AbortOnDropHandle<Result<()>> {
    AbortOnDropHandle::new(tokio::spawn(harnx_hookset_server::serve_with_shutdown(
        Arc::new(Policy { calls }),
        scope.clone(),
        harnx_nats_common::connect::NatsConnection {
            client: client.clone(),
            replicas: 1,
        },
        harnx_hookset_server::ServeLifecycle::new(shutdown.clone(), None),
    )))
}

fn spawn_operator_worker(
    global: &crate::config::GlobalConfig,
    readiness: &harnx_healthz::Readiness,
    model_calls: &Arc<AtomicUsize>,
) -> AbortOnDropHandle<Result<()>> {
    let mut daemon = super::super::WorkerDaemonConfig::new("local", "operator-worker");
    daemon.manage_servers = false;
    let count = model_calls.clone();
    let inference: crate::agent_loop::AgentCallFn = Arc::new(move |_, config, _| {
        count.fetch_add(1, Ordering::SeqCst);
        let session = config
            .read()
            .session
            .as_ref()
            .map(|session| session.id().to_owned());
        Box::pin(async move {
            anyhow::ensure!(
                session.as_deref() == Some("nested-operator"),
                "root operator must not infer"
            );
            Ok((
                String::new(),
                None,
                vec![harnx_core::tool::ToolCall::new(
                    "fixture_echo".into(),
                    json!({"text":"nested execution must await consent"}),
                    None,
                    None,
                )],
                Default::default(),
            ))
        })
    });
    AbortOnDropHandle::new(tokio::spawn(super::super::run_worker_daemon(
        global.clone(),
        daemon,
        Some(inference),
        Some(readiness.clone()),
    )))
}

async fn operator_session(client: &async_nats::Client, id: &str) -> Result<NatsSession> {
    NatsSession::new(
        NatsSessionConfig {
            cluster: "local".into(),
            initializer: SessionInitializer::named("metis", Default::default()),
            session_id: Some(id.into()),
            activation_route: SessionActivationRoute::ClusterShared,
        },
        client.clone(),
        async_nats::jetstream::new(client.clone()),
        crate::utils::create_abort_signal(),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_worker_operator_commands_preserve_active_identity_hooks_allowed_tools_and_cancel(
) -> Result<()> {
    let _lock = env_lock().await;
    let Some((url, mut nats, _store)) = spawn_test_nats().await else {
        return Ok(());
    };
    let seeded = seed_remote_config(&url);
    let _env = subagent_test_env(&url, &seeded);
    std::fs::write(seeded.config_dir().join("agents/metis.md"), "---\nmodel: test:test-model\nuse_tools: [fixture_echo, fixture_wait, fixture_delegate]\n---\nfixture agent\n")?;
    let scope = ServerScope::new();
    let _scope = TestEnvGuard::new("HARNX_SERVER_SCOPE", scope.as_str());
    let client = async_nats::connect(&url).await?;
    let fixture = LiveOperatorFixture::start(seeded.parent_config.clone(), client, scope).await?;
    let before = fixture.log().load_events_async().await?;
    fixture.wait_ready().await?;
    fixture.assert_inventory().await?;
    fixture
        .assert_active_handler(seeded.parent_config.clone())
        .await?;
    fixture.assert_root_hook_policy().await?;
    fixture.assert_cancellation().await?;
    assert_eq!(
        fixture.log().load_events_async().await?,
        before,
        "operator commands must not create inference turns"
    );
    fixture.assert_nested_consent().await?;
    fixture.close().await?;
    nats.kill()?;
    nats.wait()?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn managed_operator_invocation_starts_active_agent_hooks_and_cleans_registrations(
) -> Result<()> {
    let _lock = env_lock().await;
    let Some((url, mut nats, _store)) = spawn_test_nats().await else {
        return Ok(());
    };
    let seeded = seed_remote_config(&url);
    let _env = subagent_test_env(&url, &seeded);
    install_managed_operator_policy(seeded.config_dir())?;
    let mut config = seeded.parent_config.clone();
    config.nats_routing = NatsRouting::Default;
    config.agent = None;
    config.model = harnx_core::model::Model::new("test", "test-model");
    let global = Arc::new(crate::config::ConfigLock::new(config));
    let readiness = harnx_healthz::Readiness::default();
    let worker = AbortOnDropHandle::new(tokio::spawn(super::super::run_worker_daemon(
        global.clone(),
        super::super::WorkerDaemonConfig::managing("local", "managed-operator"),
        None,
        Some(readiness.clone()),
    )));
    tokio::time::timeout(Duration::from_secs(30), async {
        while !readiness.is_ready() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    let client = async_nats::connect(&url).await?;
    let session = NatsSession::new(
        NatsSessionConfig {
            cluster: "local".into(),
            initializer: SessionInitializer::named("metis", Default::default()),
            session_id: Some("managed-operator".into()),
            activation_route: SessionActivationRoute::ClusterShared,
        },
        client.clone(),
        async_nats::jetstream::new(client.clone()),
        crate::utils::create_abort_signal(),
    )
    .await?;
    let request = OperatorToolRequest {
        version: 1,
        session_key: session.storage_key().into(),
        use_tools: None,
        tool_use: true,
        command: OperatorToolCommand::Call {
            name: crate::session_history::TOOL_NAME.into(),
            args_json: "{}".into(),
        },
        json: true,
    };
    let reply = call(&client, &request, request.command.clone()).await;
    assert_eq!(
        reply.error.as_deref(),
        Some("agent policy denied"),
        "{reply:?}"
    );
    assert_eq!(
        serde_json::from_str::<Value>(&reply.output)?,
        json!({"error":"agent policy denied", "blocked_by_hook":true})
    );
    assert_hook_registrations_retired(&client).await?;
    assert!(global.read().tui_confirm_tool_use.is_none());
    drop(worker);
    nats.kill()?;
    nats.wait()?;
    Ok(())
}

fn install_managed_operator_policy(config_dir: &std::path::Path) -> Result<()> {
    let bin = std::env::current_exe()?
        .parent()
        .context("test binary dir")?
        .parent()
        .context("target dir")?
        .join(format!(
            "harnx-claude-compatible-hook-server{}",
            std::env::consts::EXE_SUFFIX
        ));
    ensure!(
        bin.is_file(),
        "build workspace first: {} missing",
        bin.display()
    );
    let command = managed_policy_command(
        &bin,
        json!({"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"agent policy denied"}}),
    );
    let ask_command = managed_policy_command(
        &bin,
        json!({"hookSpecificOutput":{"permissionDecision":"ask"}}),
    );
    let front = serde_yaml::to_string(
        &serde_json::json!({"model":"test:test-model", "use_tools":[crate::session_history::TOOL_NAME], "hooks":{"entries":[{"command":ask_command}, {"command":command}]}}),
    )?;
    std::fs::write(
        config_dir.join("agents/metis.md"),
        format!("---\n{front}---\noperator agent\n"),
    )?;
    Ok(())
}

async fn assert_hook_registrations_retired(client: &async_nats::Client) -> Result<()> {
    let registry = async_nats::jetstream::new(client.clone())
        .get_key_value(harnx_hookset::HOOK_REGISTRY_BUCKET)
        .await?;
    let mut keys = registry.keys().await?;
    while let Some(key) = keys.next().await {
        let key = key?;
        assert!(
            registry.get(&key).await?.is_none(),
            "agent hook registration leaked: {key}"
        );
    }
    Ok(())
}

fn managed_policy_command(bin: &std::path::Path, expression: Value) -> String {
    // Hook commands use shell_words on every platform, including Windows paths.
    shell_words::join([
        bin.to_string_lossy().into_owned(),
        "--event".into(),
        "PreToolUse".into(),
        "--jaq".into(),
        expression.to_string(),
    ])
}

#[test]
fn managed_policy_command_preserves_windows_paths_spaces_and_json() {
    let bin = std::path::Path::new(
        r"D:\a\harnx workspace\target\debug\harnx-claude-compatible-hook-server.exe",
    );
    let expression = json!({"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"agent's policy denied"}});
    let command = managed_policy_command(bin, expression.clone());
    let words = shell_words::split(&command).unwrap();
    assert_eq!(
        words,
        [
            bin.to_string_lossy().into_owned(),
            "--event".into(),
            "PreToolUse".into(),
            "--jaq".into(),
            expression.to_string(),
        ]
    );
}

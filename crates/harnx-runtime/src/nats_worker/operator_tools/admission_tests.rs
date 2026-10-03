use super::*;
use crate::nats_session_metadata::{
    AdmissionOrigin, CallTimeoutOverride, InvocationEdgeKind, RunLimitsRecord, SessionMetadataStore,
};
use crate::nats_worker::tests::{
    env_lock, seed_remote_config, spawn_test_nats, subagent_test_env, TestEnvGuard,
};
use crate::operator_tools::{
    cli::{run_reserved_tool_command, ToolCommandExecution},
    request_command, OperatorToolCommand,
};
use crate::utils::create_abort_signal;
use crate::{NatsSession, NatsSessionConfig, SessionActivationRoute, SessionInitializer};
use harnx_toolset::ToolRequest;
use serde_json::{json, Value};

struct Calls {
    client: async_nats::Client,
    config: crate::config::GlobalConfig,
    session: NatsSession,
    store: SessionMetadataStore,
    scope: harnx_core::instance::ServerScope,
    models: Arc<std::sync::atomic::AtomicUsize>,
}

impl Calls {
    async fn named(&self, command: OperatorToolCommand) -> Result<OperatorToolReply> {
        request_command(
            &self.client,
            &crate::operator_tools::subject("local", &SessionActivationRoute::ClusterShared)?,
            &OperatorToolRequest {
                version: 1,
                session_key: self.session.storage_key().into(),
                use_tools: Some(vec!["metis_session_prompt".into()]),
                tool_use: true,
                command,
                json: true,
            },
            &create_abort_signal(),
        )
        .await
    }

    async fn reserved(&self, command: OperatorToolCommand) -> Result<(OperatorToolReply, String)> {
        let view = crate::nats_worker::tool_reservation::ToolReservationView {
            package: None,
            use_tools: vec!["metis_session_prompt".into()],
        };
        let mut snapshot = self.config.read().clone();
        snapshot.nats_routing = crate::config::NatsRouting::Cluster("local".into());
        let mut reservation = crate::tool_reservation_client::ToolReservationHandle::open(
            snapshot,
            SessionActivationRoute::ClusterShared,
            view.clone(),
        )
        .await?;
        let storage = reservation.session_storage_key().to_owned();
        let result = run_reserved_tool_command(
            &reservation,
            &view,
            ToolCommandExecution {
                command: &command,
                json: true,
                abort: &create_abort_signal(),
            },
        )
        .await;
        reservation.close().await?;
        Ok((result?, storage))
    }

    async fn call(&self, named: bool) -> Result<(Value, ToolRequest, String)> {
        let mut requests = self.client.subscribe(">".to_owned()).await?;
        self.client.flush().await?;
        let command = OperatorToolCommand::Call {
            name: "metis_session_prompt".into(),
            args_json: json!({"message":"child work", "timeout_secs":604800}).to_string(),
        };
        let (reply, storage) = if named {
            (
                self.named(command).await?,
                self.session.storage_key().to_owned(),
            )
        } else {
            self.reserved(command).await?
        };
        assert!(reply.error.is_none(), "{reply:?}");
        let result: Value = serde_json::from_str(&reply.output)?;
        assert_eq!(result["response"], "child completed");
        let request = tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(message) = requests.next().await {
                if let Ok(request) = serde_json::from_slice::<ToolRequest>(&message.payload) {
                    if request.tool == "session_prompt" {
                        return Ok(request);
                    }
                }
            }
            anyhow::bail!("no actual subagent request")
        })
        .await??;
        Ok((result, request, storage))
    }

    async fn child(&self, result: &Value) -> Result<NatsSession> {
        NatsSession::new(
            NatsSessionConfig {
                cluster: "local".into(),
                initializer: SessionInitializer::named("metis", Default::default()),
                session_id: Some(
                    result["session_id"]
                        .as_str()
                        .context("actual child id")?
                        .into(),
                ),
                activation_route: SessionActivationRoute::ClusterShared,
            },
            self.client.clone(),
            async_nats::jetstream::new(self.client.clone()),
            create_abort_signal(),
        )
        .await
    }

    async fn assert_lineage(
        &self,
        result: &Value,
        request: &ToolRequest,
        storage: &str,
    ) -> Result<RunLimitsRecord> {
        let lineage = request
            .run_context
            .as_ref()
            .context("operator root lineage")?;
        let parent: RunLimitsRecord = serde_json::from_value(lineage.snapshot.clone())?;
        let child = self.child(result).await?;
        let intent = self
            .store
            .admission(child.storage_key(), &request.call_id)
            .await?
            .context("child admission")?;
        assert_eq!(intent.origin, AdmissionOrigin::Inherited);
        assert_eq!(intent.parent.as_ref(), Some(&parent));
        assert_eq!(intent.edge, Some(InvocationEdgeKind::Delegation));
        assert_eq!(
            intent.admitted_at.timestamp_millis(),
            lineage.started_at_ms as i64
        );
        let limits = self
            .store
            .get_invocation_limits(child.storage_key(), &request.call_id)
            .await?
            .context("frozen child limits")?;
        assert_eq!(limits.run_id, parent.run_id);
        assert_eq!(
            limits.deadline, parent.deadline,
            "long child override cannot renew outer run"
        );
        assert_eq!(
            limits.parent_invocation.as_ref().unwrap().invocation_id,
            parent.invocation_id
        );
        if self.config.read().run_context.is_none() {
            assert_eq!(
                self.store
                    .get_run_limits(storage, parent.run_id.as_str())
                    .await?,
                Some(parent.clone())
            );
            assert_eq!(
                self.store
                    .get_invocation_limits(storage, parent.invocation_id.as_str())
                    .await?,
                Some(parent.clone())
            );
        }
        Ok(parent)
    }

    async fn assert_inspection_without_roots(&self, named: bool) -> Result<()> {
        let mut storage = Vec::new();
        for command in [
            OperatorToolCommand::List { pattern: None },
            OperatorToolCommand::Info {
                name: "metis_session_prompt".into(),
            },
        ] {
            let (reply, key) = if named {
                (
                    self.named(command).await?,
                    self.session.storage_key().into(),
                )
            } else {
                self.reserved(command).await?
            };
            assert!(reply.error.is_none(), "{reply:?}");
            storage.push(key);
        }
        let mut keys = self.store.kv_store().keys().await?;
        while let Some(key) = keys.next().await {
            let key = key?;
            for storage in &storage {
                assert!(
                    !key.starts_with(&format!("sessions/{storage}/runs/")),
                    "inspection minted {key}"
                );
                assert!(
                    !key.starts_with(&format!("sessions/{storage}/invocations/")),
                    "inspection minted {key}"
                );
            }
        }
        assert_eq!(self.models.load(std::sync::atomic::Ordering::SeqCst), 0);
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_subagent_operator_roots_and_inherited_deadlines() -> Result<()> {
    let _lock = env_lock().await;
    let (url, mut nats, _) = spawn_test_nats().await.context("nats-server required")?;
    let seeded = seed_remote_config(&url);
    let _env = subagent_test_env(&url, &seeded);
    std::fs::write(
        seeded.config_dir().join("config.yaml"),
        "model: test:test-model\ntool_use: true\nrun_limits:\n  timeout_secs: 60\n",
    )?;
    std::fs::write(seeded.config_dir().join("agents/metis.md"), "---\nmodel: test:test-model\nrun_limits:\n  timeout_secs: 600\nuse_tools: [metis_session_prompt]\n---\nchild\n")?;
    let scope = harnx_core::instance::ServerScope::new();
    let _scope = TestEnvGuard::new("HARNX_SERVER_SCOPE", scope.as_str());
    let (calls, worker) = start_calls(seeded.config_dir(), &url, scope).await?;
    // Named inspection waits for the worker's initial background catalog.
    calls.assert_inspection_without_roots(true).await?;
    calls.assert_inspection_without_roots(false).await?;
    assert_missing_lineage(&calls).await?;
    let mut roots = Vec::new();
    for named in [false, true] {
        let (result, request, storage) = calls.call(named).await?;
        let root = calls.assert_lineage(&result, &request, &storage).await?;
        assert!(root.parent_invocation.is_none());
        assert_eq!(
            (root.deadline.unwrap() - root.admitted_at).num_seconds(),
            if named { 600 } else { 60 }
        );
        roots.push(root);
    }
    assert_ne!(roots[0].run_id, roots[1].run_id);
    let inherited = RunLimitsRecord::admit_child(
        &roots[0],
        Default::default(),
        InvocationEdgeKind::Delegation,
        chrono::Utc::now(),
        Default::default(),
        None,
        CallTimeoutOverride::Omitted,
    )?;
    calls.config.write().run_context = Some(inherited.clone());
    for named in [false, true] {
        let (result, request, storage) = calls.call(named).await?;
        assert_eq!(
            calls.assert_lineage(&result, &request, &storage).await?,
            inherited
        );
    }
    assert_expired_not_renewed(&calls, inherited).await?;
    assert_eq!(
        calls.models.load(std::sync::atomic::Ordering::SeqCst),
        4,
        "only children infer"
    );
    drop(worker);
    nats.kill()?;
    nats.wait()?;
    Ok(())
}

async fn start_calls(
    dir: &std::path::Path,
    url: &str,
    scope: harnx_core::instance::ServerScope,
) -> Result<(Calls, AbortOnDropHandle<Result<()>>)> {
    let mut config = crate::config::Config::load_from_file(&dir.join("config.yaml"))?;
    config.nats_routing = crate::config::NatsRouting::Default;
    config.model = harnx_core::model::Model::new("test", "test-model");
    let config = Arc::new(crate::config::ConfigLock::new(config));
    let client = async_nats::connect(&url).await?;
    let models = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = models.clone();
    let call: crate::agent_loop::AgentCallFn = Arc::new(move |_, _, _| {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async { Ok(("child completed".into(), None, vec![], Default::default())) })
    });
    let readiness = harnx_healthz::Readiness::default();
    let worker = AbortOnDropHandle::new(tokio::spawn(super::super::run_worker_daemon(
        config.clone(),
        super::super::WorkerDaemonConfig::new("local", "admission-operator"),
        Some(call),
        Some(readiness.clone()),
    )));
    tokio::time::timeout(Duration::from_secs(30), async {
        while !readiness.is_ready() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    let session = NatsSession::new(
        NatsSessionConfig {
            cluster: "local".into(),
            initializer: SessionInitializer::named("metis", Default::default()),
            session_id: Some("operator-admission".into()),
            activation_route: SessionActivationRoute::ClusterShared,
        },
        client.clone(),
        async_nats::jetstream::new(client.clone()),
        create_abort_signal(),
    )
    .await?;
    let store = session.metadata_store().clone();
    let calls = Calls {
        client,
        config,
        session,
        store,
        scope,
        models,
    };
    crate::nats_session_log::NatsSessionLog::new(
        calls.session.jetstream().clone(),
        calls.session.storage_key(),
    )
    .last_entry_async()
    .await?;
    Ok((calls, worker))
}

async fn assert_missing_lineage(calls: &Calls) -> Result<()> {
    let snapshot = calls.config.read().clone();
    let provider = crate::nats_tool_provider::NatsToolProvider::discover_strict(
        &snapshot,
        calls.scope.clone(),
        Default::default(),
        None,
    )
    .await?;
    let ctx = crate::tool::build_tool_eval_context_with_provider(
        crate::tool_context::BuildToolEvalContextParams::new(&calls.config, &calls.scope)
            .with_agent_use_tools(Some("metis_session_prompt")),
        Arc::new(provider),
    )
    .await;
    let reply = crate::operator_tools::evaluate(
        ctx,
        &OperatorToolCommand::Call {
            name: "metis_session_prompt".into(),
            args_json: json!({"message":"no authority"}).to_string(),
        },
        crate::tool_output::ToolOutputFormat::Json,
        &create_abort_signal(),
    )
    .await;
    let error = reply.expect_err("missing lineage must deny actual subagent execution");
    assert!(
        error
            .to_string()
            .contains("sub-agent execution requires inherited run context"),
        "{error:#}"
    );
    assert_eq!(calls.models.load(std::sync::atomic::Ordering::SeqCst), 0);
    Ok(())
}

async fn assert_expired_not_renewed(calls: &Calls, mut inherited: RunLimitsRecord) -> Result<()> {
    inherited.deadline = Some(chrono::Utc::now() - chrono::Duration::seconds(1));
    calls.config.write().run_context = Some(inherited.clone());
    let command = OperatorToolCommand::Call {
        name: "metis_session_prompt".into(),
        args_json: json!({"message":"expired"}).to_string(),
    };
    for named in [false, true] {
        let requests = calls.client.subscribe(">".to_owned()).await?;
        calls.client.flush().await?;
        let error = if named {
            calls
                .named(command.clone())
                .await?
                .error
                .expect("expired named call denied")
        } else {
            calls
                .reserved(command.clone())
                .await
                .expect_err("expired reservation call denied")
                .to_string()
        };
        assert!(
            error.contains("Invocation deadline expired before dispatch"),
            "{error}"
        );
        assert_no_tool_dispatch(&calls.client, requests).await?;
        assert_eq!(calls.config.read().run_context.as_ref(), Some(&inherited));
    }
    Ok(())
}

async fn assert_no_tool_dispatch(
    client: &async_nats::Client,
    mut requests: async_nats::Subscriber,
) -> Result<()> {
    let fence = format!("expired.fence.{}", uuid::Uuid::new_v4());
    client.publish(fence.clone(), "".into()).await?;
    client.flush().await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(message) = requests.next().await {
            if message.subject.as_str() == fence {
                break;
            }
            assert!(
                serde_json::from_slice::<ToolRequest>(&message.payload).is_err(),
                "expired run dispatched a tool"
            );
        }
    })
    .await?;
    Ok(())
}

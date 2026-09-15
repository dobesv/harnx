use super::*;
use crate::execution_fence::GenerationFence;
use crate::nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig};
use crate::nats_session_log::{recovery::prompt_owner, NatsSessionLog};
use crate::nats_worker::execution_control::WorkerExecution;
use harnx_execution_control::{ExecutionStore, InterruptScope, Interrupted, Owner};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Fixture {
    _server: crate::nats_test_common::NatsServerHandle,
    js: jetstream::Context,
    log: NatsSessionLog,
    fence: GenerationFence,
    lease: NatsSessionLease,
    scope: harnx_core::instance::ServerScope,
    invoked: Arc<AtomicUsize>,
}

impl Fixture {
    async fn new() -> Result<Self> {
        let server = crate::nats_test_common::spawn_nats_server()
            .await?
            .context("nats-server required")?;
        let js = jetstream::new(async_nats::connect(server.url()).await?);
        let lease = acquire(&js, "first").await?;
        let store = ExecutionStore::ensure(&js, 1).await?;
        let op = store.session("recovery", None, Some("g1")).await?;
        store
            .claim(
                &op.reference,
                Owner {
                    instance_id: lease.worker_id().into(),
                    fence: lease.fence_token(),
                },
            )
            .await?;
        let ctx = store.activate_gate(&op.reference).await?;
        Ok(Self {
            _server: server,
            log: NatsSessionLog::new(js.clone(), "recovery"),
            js,
            fence: GenerationFence::new(store, ctx),
            lease,
            scope: harnx_core::instance::ServerScope::new(),
            invoked: Arc::new(AtomicUsize::new(0)),
        })
    }

    async fn prompt(&self, commit_sequence: bool) -> Result<u64> {
        self.fence
            .store
            .reserve_prompt(self.fence.context.generation(), "prompt")
            .await?;
        let seq = self.log.append_event_async(&user()).await?;
        if commit_sequence {
            self.fence
                .store
                .commit_prompt(self.fence.context.generation(), "prompt", seq)
                .await?;
        }
        Ok(seq)
    }

    async fn calls(&self, proved: bool) -> Result<PendingToolCalls> {
        let entry = SessionLogEntry::ToolCalls {
            text: String::new(),
            thought: None,
            calls: vec![harnx_core::tool::ToolCall::new(
                "read".into(),
                serde_json::json!({}),
                Some("call".into()),
                None,
            )],
            timestamp: None,
            fence_token: Some(self.lease.fence_token()),
        };
        let seq = if proved {
            self.log
                .append_output(&self.fence, &entry, None)
                .await?
                .context("call sequence")?
        } else {
            self.log.append_event_async(&entry).await?
        };
        Ok(find_orphan_tool_calls(&[(seq, entry)]).pop().unwrap())
    }

    async fn stop(&self) -> Result<()> {
        self.fence
            .store
            .interrupt(
                &InterruptScope {
                    gate_root: self.fence.context.gate_root().clone(),
                    operation: self.fence.context.operation().clone(),
                    reason: "user stopped".into(),
                },
                "stop",
            )
            .await?;
        Ok(())
    }

    async fn restart(&self) -> Result<WorkerExecution> {
        // Crash seam: no owner_stopped, Cancel projection or descendant wake.
        self.lease.release().await?;
        let lease = acquire(&self.js, "replacement").await?;
        let store = ExecutionStore::ensure(&self.js, 1).await?;
        let mut activation = super::super::SessionActivate::new("recovery");
        let execution = WorkerExecution::claim(store, &mut activation, &lease, &self.js).await?;
        lease.release().await?;
        Ok(execution)
    }

    async fn repair(
        &self,
        orphan: &PendingToolCalls,
        fence: GenerationFence,
    ) -> Result<Vec<harnx_core::session::ToolOutput>> {
        let config = Arc::new(parking_lot::RwLock::new(crate::config::Config::default()));
        config.write().generation_fence = Some(fence.clone());
        let mut repair = build_tool_repair_context(&config);
        let mut declaration: harnx_core::tool::ToolDeclaration = serde_json::from_value(
            serde_json::json!({
                "name": "read", "description": "", "parameters": {"type": "object"}, "idempotent_hint": true
            }),
        )?;
        declaration.idempotent_hint = Some(true);
        repair.decl_map.insert("read".into(), declaration);
        let mut eval = tool_recovery::tests::mixed_context(self.scope.clone());
        eval.providers = vec![Arc::new(CountingTool(self.invoked.clone()))];
        eval.work_boundary = crate::execution_fence::tool_boundary(Some(fence));
        let abort = crate::utils::create_abort_signal();
        repair_single_orphan(
            orphan,
            &RepairOrphanToolCallsArgs {
                log: &self.log,
                config,
                instance_id: &self.scope,
                fence_token: None,
                worker_id: None,
                session_id: "recovery",
                abort_signal: &abort,
                lease: None,
            },
            &repair,
            &eval,
        )
        .await
    }
}

async fn acquire(js: &jetstream::Context, worker: &str) -> Result<NatsSessionLease> {
    NatsSessionLease::acquire(NatsLeaseAcquireParams {
        jetstream: js.clone(),
        session_id: "recovery",
        worker_id: worker.into(),
        generation: 1,
        config: NatsLeaseConfig::default(),
        session_metadata: None,
    })
    .await?
    .context("lease")
}

fn user() -> SessionLogEntry {
    SessionLogEntry::Message {
        id: Some("prompt".into()),
        role: harnx_core::message::MessageRole::User,
        content: harnx_core::message::MessageContent::Text("read the file".into()),
        timestamp: None,
        fence_token: None,
    }
}

struct CountingTool(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl harnx_core::tool::ToolProvider for CountingTool {
    fn name(&self) -> &str {
        "counter"
    }
    fn has_tool(&self, _: &str) -> bool {
        true
    }
    async fn call_tool(
        &self,
        _: &str,
        _: serde_json::Value,
        _: &AbortSignal,
    ) -> std::result::Result<harnx_core::tool::ToolProviderOutput, harnx_core::tool::ToolError>
    {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(harnx_core::tool::ToolProviderOutput::new(
            serde_json::json!("read"),
        ))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_after_acceptance_repairs_cancel_before_worker_reconstruction() -> Result<()> {
    let f = Fixture::new().await?;
    f.prompt(false).await?; // Also lose the append acknowledgement.
    let orphan = f.calls(true).await?;
    f.stop().await?;
    assert!(!f
        .log
        .load_events_latest_async()
        .await?
        .iter()
        .any(|(_, e)| matches!(e, SessionLogEntry::Cancel { .. })));
    let execution = f.restart().await?;
    assert_eq!(execution.reference, *f.fence.context.generation());
    assert!(execution.cancelled().await?);
    let error = f
        .repair(&orphan, execution.fence.unwrap())
        .await
        .unwrap_err();
    assert!(error.is::<Interrupted>(), "{error:#}");
    let entries = f.log.load_events_latest_async().await?;
    assert!(find_orphan_tool_calls(&entries).is_empty());
    assert!(matches!(
        entries.last(),
        Some((_, SessionLogEntry::Cancel { .. }))
    ));
    assert!(!entries
        .iter()
        .any(|(_, e)| matches!(e, SessionLogEntry::ToolResults { .. })));
    assert_eq!(f.invoked.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_orphan_never_enters_idempotent_partition_after_stop() -> Result<()> {
    let f = Fixture::new().await?;
    f.prompt(true).await?;
    let orphan = f.calls(false).await?;
    f.stop().await?;
    let error = f.repair(&orphan, f.fence.clone()).await.unwrap_err();
    assert!(error.is::<Interrupted>(), "{error:#}");
    let execution = f.restart().await?;
    assert!(execution.cancelled().await?);
    assert_eq!(f.invoked.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_without_interrupt_preserves_legacy_resume_and_original_prompt() -> Result<()> {
    let f = Fixture::new().await?;
    f.prompt(true).await?;
    let orphan = f.calls(false).await?;
    let execution = f.restart().await?;
    assert!(!execution.cancelled().await?);
    assert_eq!(execution.reference, *f.fence.context.generation());
    let results = f.repair(&orphan, execution.fence.unwrap()).await?;
    assert_eq!(results[0].output, "read");
    assert_eq!(f.invoked.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_legacy_call_authority_never_invokes_tool() -> Result<()> {
    let f = Fixture::new().await?;
    let seq = f
        .log
        .append_event_async(&SessionLogEntry::ToolCalls {
            text: String::new(),
            thought: None,
            calls: vec![harnx_core::tool::ToolCall::new(
                "read".into(),
                serde_json::json!({}),
                None,
                None,
            )],
            timestamp: None,
            fence_token: None,
        })
        .await?;
    let entries = f.log.load_events_latest_async().await?;
    let orphan = find_orphan_tool_calls(&entries).pop().unwrap();
    assert_eq!(orphan.seq, seq);
    let error = f.repair(&orphan, f.fence.clone()).await.unwrap_err();
    assert!(error.to_string().contains("unknown legacy"), "{error:#}");
    assert_eq!(f.invoked.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopped_prompt_and_legacy_call_keep_g1_after_physical_retirement_and_g2() -> Result<()> {
    let f = Fixture::new().await?;
    let prompt_seq = f.prompt(false).await?;
    let orphan = f.calls(false).await?;
    f.stop().await?;
    // Retire without a transcript Cancel, as if cleanup survived but projector died.
    f.fence
        .store
        .cancel_operation(f.fence.context.operation(), Some("stop"), false)
        .await?;
    f.fence
        .store
        .mutate(f.fence.context.operation(), |op| {
            op.transition(harnx_execution_control::OperationState::Unconfirmed)
        })
        .await?;
    f.fence.store.abandon_unconfirmed("recovery", "g1").await?;
    let g2 = f.fence.store.session("recovery", None, Some("g2")).await?;
    assert!(f
        .fence
        .store
        .get(f.fence.context.operation())
        .await?
        .is_none());
    f.fence
        .store
        .claim(
            &g2.reference,
            Owner {
                instance_id: "g2-owner".into(),
                fence: 100,
            },
        )
        .await?;
    let ctx = f.fence.store.activate_gate(&g2.reference).await?;
    let g2_fence = GenerationFence::new(f.fence.store.clone(), ctx);
    let history = f.fence.store.recovery_history("recovery").await?;
    assert_eq!(
        prompt_owner(&history, Some("prompt"), prompt_seq)?
            .unwrap()
            .reference,
        *f.fence.context.generation()
    );
    let error = f.log.admit_reconstruction(&g2_fence).await.unwrap_err();
    assert!(error.is::<Interrupted>(), "{error:#}");
    let error = f.repair(&orphan, g2_fence).await.unwrap_err();
    assert!(error.is::<Interrupted>(), "{error:#}");
    assert!(f
        .fence
        .store
        .get(&g2.reference)
        .await?
        .unwrap()
        .admissions
        .is_empty());
    assert_eq!(f.invoked.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_pending_prompt_is_not_automatically_adopted() -> Result<()> {
    let f = Fixture::new().await?;
    f.log.append_event_async(&user()).await?;
    let error = match f.restart().await {
        Ok(_) => anyhow::bail!("unbound prompt adopted"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("automatic adoption refused"),
        "{error:#}"
    );
    assert!(f
        .fence
        .store
        .get(f.fence.context.operation())
        .await?
        .unwrap()
        .admissions
        .is_empty());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inherited_stop_before_first_child_claim_projects_control_without_model() -> Result<()> {
    use harnx_execution_control::OperationRef;
    let f = Fixture::new().await?;
    let tool = f
        .fence
        .store
        .child(
            OperationRef::new("recovery", "tool"),
            f.fence.context.operation().clone(),
        )
        .await?;
    f.fence
        .store
        .claim(&tool.reference, Owner::invocation("tool-server"))
        .await?;
    f.fence.store.activate_gate(&tool.reference).await?;
    let child = f
        .fence
        .store
        .session("child", Some(tool.reference), Some("child-g1"))
        .await?;
    let child_log = NatsSessionLog::new(f.js.clone(), "child");
    f.fence
        .store
        .reserve_prompt(&child.reference, "prompt")
        .await?;
    let seq = child_log.append_event_async(&user()).await?;
    f.fence
        .store
        .commit_prompt(&child.reference, "prompt", seq)
        .await?;
    f.stop().await?; // No descendant wake or physical state update.
    let lease = NatsSessionLease::acquire(NatsLeaseAcquireParams {
        jetstream: f.js.clone(),
        session_id: "child",
        worker_id: "child-worker".into(),
        generation: 1,
        config: Default::default(),
        session_metadata: None,
    })
    .await?
    .context("child lease")?;
    let mut activation = super::super::SessionActivate::new("child");
    let execution =
        WorkerExecution::claim(f.fence.store.clone(), &mut activation, &lease, &f.js).await?;
    assert!(execution.cancelled().await?);
    let entries = child_log.load_events_latest_async().await?;
    assert!(matches!(
        entries.last(),
        Some((_, SessionLogEntry::Cancel { .. }))
    ));
    let error = execution.fence.unwrap().check("model").await.unwrap_err();
    assert!(error.is::<Interrupted>());
    lease.release().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reserved_prompt_sequence_is_backfilled_on_normal_restart() -> Result<()> {
    let f = Fixture::new().await?;
    let seq = f.prompt(false).await?;
    let execution = f.restart().await?;
    assert!(!execution.cancelled().await?);
    let history = f.fence.store.recovery_history("recovery").await?;
    assert_eq!(
        prompt_owner(&history, Some("prompt"), seq)?
            .unwrap()
            .admissions["prompt"],
        Some(seq)
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn modified_orphan_does_not_borrow_original_round_authority() -> Result<()> {
    let f = Fixture::new().await?;
    let mut orphan = f.calls(true).await?;
    orphan.calls[0].arguments = serde_json::json!({"changed": true});
    let error = f.repair(&orphan, f.fence.clone()).await.unwrap_err();
    assert!(
        error.to_string().contains("unknown recovery authority"),
        "{error:#}"
    );
    assert_eq!(f.invoked.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_claim_does_not_bind_unknown_history_using_its_new_fence() -> Result<()> {
    let f = Fixture::new().await?;
    let orphan = f.calls(false).await?;
    let error = f.repair(&orphan, f.fence.clone()).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unknown legacy predecessor fence"),
        "{error:#}"
    );
    assert_eq!(f.invoked.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hitl_managed_orphan_cannot_bypass_recovery_ownership() -> Result<()> {
    let f = Fixture::new().await?;
    f.calls(false).await?;
    f.log
        .append_event_async(&SessionLogEntry::HitlApprovalRequested {
            tool_call_id: "call".into(),
            summary: "read".into(),
            fence_token: f.lease.fence_token(),
        })
        .await?;
    let entries = f.log.load_events_latest_async().await?;
    assert_eq!(derive_pending_hitl_approvals(&entries)?.len(), 1);
    let config = Arc::new(parking_lot::RwLock::new(crate::config::Config::default()));
    config.write().generation_fence = Some(f.fence.clone());
    let backend =
        NatsSessionLogBackend::new(f.js.clone(), "recovery").with_execution(Some(f.fence.clone()));
    let error = tool_recovery::admit_pending_rounds(&backend, &config, &entries)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unknown legacy predecessor fence"),
        "{error:#}"
    );
    assert_eq!(f.invoked.load(Ordering::SeqCst), 0);
    Ok(())
}

use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use harnx_core::{
    abort::create_abort_signal,
    hooks::{HookEvent, HookOutcome, HookResult, HookResultControl},
    instance::ServerScope,
    partial_result::error_with_partial_result,
    tool::{ToolCallOrigin, ToolError, ToolProgress, ToolProvider, ToolProviderOutput},
};
use harnx_engine::tool::ToolEvalContext;
use harnx_script::{
    commands::{tool_commands, ToolSpec},
    BashKitBackend, ScriptBackend, ScriptLimits, ScriptRun,
};
use serde_json::json;
use tokio::sync::{mpsc, Semaphore};

use super::*;
use crate::bash_inner::{InnerCallExecutor, InnerCallExecutorConfig, InnerTraceStatus};

mod assertions;
use assertions::{
    assert_adapter_cancelled, assert_finished_concurrency, assert_round_calls, assert_trace,
};

#[derive(Default)]
struct Provider {
    calls: Mutex<Vec<(String, Value, Option<u64>)>>,
    active: AtomicUsize,
    maximum: AtomicUsize,
    gate: Option<(mpsc::UnboundedSender<String>, Arc<Semaphore>)>,
}

struct Active<'a>(&'a AtomicUsize);
impl Drop for Active<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl ToolProvider for Provider {
    fn name(&self) -> &str {
        "script-adapter-fake"
    }
    fn has_tool(&self, _name: &str) -> bool {
        true
    }
    async fn call_tool(
        &self,
        name: &str,
        args: Value,
        abort: &AbortSignal,
    ) -> Result<ToolProviderOutput, ToolError> {
        self.call_tool_with_progress(
            name,
            args,
            ToolCallOrigin::default(),
            abort,
            Arc::new(NoProgress),
        )
        .await
    }
    async fn call_tool_with_progress(
        &self,
        name: &str,
        args: Value,
        origin: ToolCallOrigin<'_>,
        _abort: &AbortSignal,
        _progress: Arc<dyn ToolProgress>,
    ) -> Result<ToolProviderOutput, ToolError> {
        self.calls
            .lock()
            .unwrap()
            .push((name.into(), args.clone(), origin.tool_round));
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.maximum.fetch_max(active, Ordering::SeqCst);
        let _active = Active(&self.active);
        if let Some((started, gate)) = &self.gate {
            started.send(name.into()).unwrap();
            gate.acquire().await.unwrap().forget();
        }
        match name {
            "fatal" => Err(ToolError::Fatal(anyhow::anyhow!("provider fatal"))),
            "broken" => Err(ToolError::Recoverable(error_with_partial_result(
                anyhow::anyhow!("provider failure"),
                json!({"partial":42}),
            ))),
            "domain" => Ok(ToolProviderOutput::new(
                json!({"content":[{"type":"text","text":"domain failure"}],"isError":true}),
            )),
            "fs_read" => Ok(ToolProviderOutput::new(
                json!({"content":[{"type":"text","text":"fallback"}],"structuredContent":{"path":args["path"]}}),
            )),
            _ => Ok(ToolProviderOutput::new(json!({"name":name}))),
        }
    }
}

struct NoProgress;
impl ToolProgress for NoProgress {
    fn update(&self, _patch: harnx_core::tool::ToolUpdatePatch) {}
}

fn context(
    provider: Arc<Provider>,
    control: HookResultControl,
    hooks: Arc<Mutex<Vec<HookEvent>>>,
) -> ToolEvalContext {
    ToolEvalContext {
        work_boundary: None,
        instance_id: ServerScope::new(),
        tool_round: Some(1),
        render: None,
        providers: vec![provider],
        allowed_tool_names: HashSet::new(),
        current_agent_package: None,
        handoff_targets: HashMap::new(),
        emit_tool_call_fn: Arc::new(|_, _| panic!("outer lifecycle leaked")),
        emit_tool_result_fn: Arc::new(|_, _| panic!("outer lifecycle leaked")),
        emit_tool_blocked_fn: Arc::new(|_, _| panic!("outer lifecycle leaked")),
        emit_tool_update_fn: Arc::new(|_, _| panic!("outer progress leaked")),
        confirm_tool_use_fn: Arc::new(|_, _, _| panic!("outer confirmation leaked")),
        dispatch_hook_fn: Arc::new(move |event| {
            hooks.lock().unwrap().push(event);
            let outcome = HookOutcome {
                control: control.clone(),
                result: HookResult::default(),
            };
            Box::pin(async move { outcome })
        }),
        prepare_handoff_fn: None,
    }
}

fn adapter(
    provider: Arc<Provider>,
    control: HookResultControl,
    cap: usize,
    budget: usize,
) -> (
    Arc<InnerCallRun>,
    Arc<ScriptInnerExecutor>,
    Arc<Mutex<Vec<HookEvent>>>,
) {
    let hooks = Arc::default();
    let ctx = context(provider.clone(), control, Arc::clone(&hooks));
    let executor = InnerCallExecutor::new(
        InnerCallExecutorConfig::new(
            ["a", "b", "approval", "fatal", "broken", "domain", "fs_read"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            vec![provider],
        )
        .with_caps(cap, budget),
    );
    let run = Arc::new(executor.begin_run(&ctx, Some(42), None));
    let adapter = Arc::new(ScriptInnerExecutor::new(run.clone(), create_abort_signal()));
    (run, adapter, hooks)
}

async fn script(script: &str, executor: Arc<ScriptInnerExecutor>) -> ScriptRun {
    let tools = ["a", "b", "approval", "fatal", "broken", "domain", "fs_read"]
        .into_iter()
        .map(|name| ToolSpec {
            name: name.into(),
            description: format!("Call {name}."),
            schema: if name == "fs_read" {
                json!({"properties":{"path":{"type":"string"}},"required":["path"]})
            } else {
                json!({"properties":{}})
            },
        })
        .collect();
    BashKitBackend
        .run(harnx_script::ScriptRequest {
            script,
            limits: ScriptLimits {
                timeout_secs: 5,
                ..Default::default()
            },
            commands: tool_commands(tools, executor).unwrap(),
            fs_mounts: vec![],
            cancel: CancellationToken::new(),
        })
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn background_commands_through_adapter_respect_concurrency_cap() {
    let (started, mut starts) = mpsc::unbounded_channel();
    let release = Arc::new(Semaphore::new(0));
    let provider = Arc::new(Provider {
        gate: Some((started, release.clone())),
        ..Default::default()
    });
    let (run, adapter, hooks) = adapter(provider.clone(), HookResultControl::Continue, 2, 100);
    let task = tokio::spawn(script("a & b & a & b & a & b & wait", adapter));
    for _ in 0..2 {
        tokio::time::timeout(Duration::from_secs(2), starts.recv())
            .await
            .unwrap()
            .unwrap();
    }
    assert_eq!(
        provider.active.load(Ordering::SeqCst),
        2,
        "commands must overlap"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), starts.recv())
            .await
            .is_err(),
        "third provider bypassed cap"
    );
    release.add_permits(6);
    let result = tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.exit_code, 0, "{result:?}");
    assert_finished_concurrency(&provider, 2);
    assert_round_calls(&provider, 6);
    assert_trace(&run, 6, InnerTraceStatus::Ok);
    assert_eq!(hooks.lock().unwrap().len(), 12, "one pre/post per call");
}

#[tokio::test]
async fn adapter_preserves_pipeline_values_errors_and_fatal_semantics() {
    for (command, code, expected) in [
        ("fs_read --path a | jq -r '.path'", 0, "a\n"),
        ("broken; echo unreachable", 1, "provider failure"),
        ("domain; echo unreachable", 1, "domain failure"),
        ("set +e; fatal; echo unreachable", 1, "provider fatal"),
    ] {
        let provider = Arc::new(Provider::default());
        let (run, adapter, _) = adapter(provider.clone(), HookResultControl::Continue, 2, 100);
        let result = script(command, adapter).await;
        assert_eq!(result.exit_code, code, "{result:?}");
        assert!(
            if code == 0 {
                result.stdout.contains(expected)
            } else {
                result.stderr.contains(expected)
            },
            "{result:?}"
        );
        assert!(!result.stdout.contains("unreachable"));
        assert_eq!(provider.calls.lock().unwrap().len(), 1);
        assert_eq!(run.trace().len(), 1);
        if command.starts_with("broken") {
            assert_eq!(result.stdout, "{\"partial\":42}\n");
        }
    }
}

#[tokio::test]
async fn approval_and_blocked_commands_stop_without_provider_dispatch() {
    for (control, code, expected) in [
        (
            HookResultControl::Ask {
                reason: Some("approval".into()),
            },
            3,
            "needs approval: approval; call it directly instead of from bash",
        ),
        (
            HookResultControl::Block {
                reason: "blocked by policy".into(),
            },
            4,
            "blocked by policy",
        ),
    ] {
        let provider = Arc::new(Provider::default());
        let (run, adapter, hooks) = adapter(provider.clone(), control, 2, 100);
        let result = script("approval; echo unreachable", adapter).await;
        assert_eq!(result.exit_code, code, "{result:?}");
        assert!(result.stderr.contains(expected));
        assert!(result.stdout.is_empty());
        assert!(provider.calls.lock().unwrap().is_empty());
        assert_eq!(run.trace().len(), 1);
        assert_eq!(hooks.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn adapter_uses_typed_rejection_kinds_and_shared_call_budget() {
    let (run, adapter, _) = adapter(Arc::default(), HookResultControl::Continue, 1, 1);
    let result = script("a; b; echo unreachable", adapter.clone()).await;
    assert_eq!(result.exit_code, 5, "{result:?}");
    assert!(!result.stdout.contains("unreachable"));
    let outcome = adapter
        .call("not_allowed", json!({}), CancellationToken::new())
        .await;
    assert!(matches!(
        outcome,
        CallOutcome::Rejected {
            kind: RejectionKind::UnknownCommand,
            ..
        }
    ));
    assert_eq!(run.trace().len(), 3);
}

#[tokio::test]
async fn cancellation_drops_queued_and_active_calls_without_aborting_parent() {
    let (started, mut starts) = mpsc::unbounded_channel();
    let provider = Arc::new(Provider {
        gate: Some((started, Arc::new(Semaphore::new(0)))),
        ..Default::default()
    });
    let (run, adapter, _) = adapter(provider.clone(), HookResultControl::Continue, 1, 100);
    let cancel = CancellationToken::new();
    let first_adapter = adapter.clone();
    let first_cancel = cancel.clone();
    let first = tokio::spawn(async move { first_adapter.call("a", json!({}), first_cancel).await });
    tokio::time::timeout(Duration::from_secs(2), starts.recv())
        .await
        .unwrap()
        .unwrap();
    let second_adapter = adapter.clone();
    let second_cancel = cancel.clone();
    let second =
        tokio::spawn(async move { second_adapter.call("b", json!({}), second_cancel).await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while run.issued.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("second call must queue behind the active provider");
    cancel.cancel();
    assert!(matches!(first.await.unwrap(), CallOutcome::Fatal { .. }));
    assert!(matches!(second.await.unwrap(), CallOutcome::Fatal { .. }));
    assert_adapter_cancelled(&run, &adapter, &provider);
    adapter.abort.set_ctrlc();
    let outcome = adapter.call("a", json!({}), CancellationToken::new()).await;
    assert!(matches!(outcome, CallOutcome::Fatal { .. }));
}

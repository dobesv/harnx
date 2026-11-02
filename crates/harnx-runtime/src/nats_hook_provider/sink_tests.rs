use super::*;
use crate::bash_inner::{InnerCallExecutor, InnerCallExecutorConfig, InnerOutcome};
use harnx_core::abort::{create_abort_signal, AbortSignal};
use harnx_core::event::AgentEventSink;
use harnx_core::tool::{ToolError, ToolProvider, ToolProviderOutput};
use harnx_engine::tool::ToolEvalContext;
use serde_json::{json, Value};
use tokio::sync::{mpsc, Barrier};

struct NoticeSink(mpsc::UnboundedSender<String>);

impl AgentEventSink for NoticeSink {
    fn emit(&self, event: AgentEvent) {
        let AgentEvent::Notice(NoticeEvent::Error(message)) = event else {
            panic!("unexpected event: {event:?}");
        };
        self.0.send(message).expect("notice receiver open");
    }
}

struct PostHookDispatcher {
    release: Arc<Barrier>,
}

#[async_trait]
impl HookRequestDispatcher for PostHookDispatcher {
    async fn request(
        &self,
        _subject: String,
        payload: Vec<u8>,
        _options: HookRequestOptions,
    ) -> Result<HookOutcome> {
        let payload: HookPayload = serde_json::from_slice(&payload)?;
        // Hold notices until both inner eval tasks and their sink scopes have ended.
        self.release.wait().await;
        Ok(HookOutcome {
            control: HookResultControl::Block {
                reason: format!("{}:{}", payload.session_id, payload.hook_event.event_name()),
            },
            result: HookResult::default(),
        })
    }
}

struct SuccessOrFailure;

#[async_trait]
impl ToolProvider for SuccessOrFailure {
    fn name(&self) -> &str {
        "success-or-failure"
    }

    fn has_tool(&self, name: &str) -> bool {
        matches!(name, "succeed" | "fail")
    }

    async fn call_tool(
        &self,
        name: &str,
        _arguments: Value,
        _abort: &AbortSignal,
    ) -> Result<ToolProviderOutput, ToolError> {
        match name {
            "succeed" => Ok(ToolProviderOutput::new(json!({"ok": true}))),
            "fail" => Err(ToolError::Recoverable(anyhow::anyhow!("tool failed"))),
            _ => panic!("unexpected tool: {name}"),
        }
    }
}

fn context(hooks: Arc<NatsHookProvider>, session: &str) -> ToolEvalContext {
    let meta = HookDispatchMeta {
        abort: None,
        session_id: session.to_string(),
        cwd: PathBuf::from("/tmp"),
        resume_count: 0,
    };
    ToolEvalContext {
        work_boundary: None,
        instance_id: ServerScope::new(),
        tool_round: Some(42),
        render: None,
        providers: vec![],
        allowed_tool_names: Default::default(),
        current_agent_package: None,
        handoff_targets: Default::default(),
        emit_tool_call_fn: Arc::new(|_, _| panic!("outer Started callback")),
        emit_tool_result_fn: Arc::new(|_, _| panic!("outer Completed callback")),
        emit_tool_blocked_fn: Arc::new(|_, _| panic!("outer Blocked callback")),
        emit_tool_update_fn: Arc::new(|_, _| panic!("outer progress callback")),
        confirm_tool_use_fn: Arc::new(|_, _, _| panic!("unexpected confirmation")),
        dispatch_hook_fn: Arc::new(move |event| {
            let hooks = Arc::clone(&hooks);
            let meta = meta.clone();
            Box::pin(async move { hooks.dispatch_event(event, None, meta).await })
        }),
        prepare_handoff_fn: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawned_inner_eval_post_hook_notices_keep_sessions_isolated() {
    tokio::time::timeout(Duration::from_secs(5), async {
        // No process-global sink: only begin_run's scope supplies each session's sink.
        assert!(current_agent_event_sink().is_none());
        let release = Arc::new(Barrier::new(5));
        let hooks = Arc::new(NatsHookProvider::from_dispatcher(
            ServerScope::new(),
            ["PostToolUse", "PostToolUseFailure"]
                .into_iter()
                .map(|event| DiscoveredHook {
                    server: "post-hook".into(),
                    display_label: None,
                    spec: HookSpec {
                        event: event.into(),
                        matcher: Some(".*".into()),
                        priority: 0,
                        timeout_secs: None,
                        fail_policy: FailPolicy::Closed,
                    },
                })
                .collect(),
            Arc::new(PostHookDispatcher {
                release: Arc::clone(&release),
            }),
        ));
        let executor = InnerCallExecutor::new(InnerCallExecutorConfig::new(
            vec!["succeed".into(), "fail".into()],
            vec![Arc::new(SuccessOrFailure)],
        ));
        let mut tasks = Vec::new();
        let mut receivers = Vec::new();
        for session in ["session-a", "session-b"] {
            let (sender, receiver) = mpsc::unbounded_channel();
            let sink: Arc<dyn AgentEventSink> = Arc::new(NoticeSink(sender));
            let ctx = context(Arc::clone(&hooks), session);
            let run = with_agent_event_sink(sink, async {
                executor.begin_run(&ctx, Some(42), None)
            })
            .await;
            receivers.push((session, receiver));
            // No sink is inherited by the BashKit-style command task. InnerCallRun
            // must re-scope it, then NatsHookProvider must cross a second spawn boundary.
            tasks.push(tokio::spawn(async move {
                assert!(current_agent_event_sink().is_none());
                let abort = create_abort_signal();
                assert!(matches!(
                    run.call("succeed", json!({}), &abort).await,
                    InnerOutcome::Ok { .. }
                ));
                assert!(matches!(
                    run.call("fail", json!({}), &abort).await,
                    InnerOutcome::ToolError { .. }
                ));
            }));
        }
        for task in tasks {
            task.await.expect("inner eval task");
        }
        assert!(current_agent_event_sink().is_none());
        for (_, receiver) in &mut receivers {
            assert!(receiver.try_recv().is_err(), "hook replied before release");
        }
        release.wait().await;
        for (session, mut receiver) in receivers {
            let mut messages = Vec::new();
            while let Some(message) = receiver.recv().await {
                messages.push(message);
            }
            messages.sort();
            let mut expected: Vec<_> = ["PostToolUse", "PostToolUseFailure"]
                .into_iter()
                .map(|event| {
                    format!(
                        "post-hook hook returned Block {{ reason: \"{session}:{event}\" }} during best-effort {event} dispatch"
                    )
                })
                .collect();
            expected.sort();
            assert_eq!(messages, expected, "notices for {session}");
        }
    })
    .await
    .expect("post-hook notices must reach both sessions");
}

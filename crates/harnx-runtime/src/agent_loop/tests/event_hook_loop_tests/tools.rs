use super::*;
use std::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

#[derive(Default)]
struct MetadataLog {
    log: PromptLog,
    calls: AtomicUsize,
    cancelled: AtomicUsize,
    hang_first: bool,
}

struct Pending<'a>(&'a AtomicUsize);
impl Drop for Pending<'_> {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl SessionAppendSink for MetadataLog {
    fn append(&self, entry: &SessionLogEntry) -> Result<u64> {
        self.log.append(entry)
    }

    fn load_metadata(
        &self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Option<crate::nats_session_metadata::SessionMetadata>>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let index = self.calls.fetch_add(1, Ordering::SeqCst);
            if self.hang_first && index == 0 {
                let _pending = Pending(&self.cancelled);
                std::future::pending::<()>().await;
            }
            Ok(None)
        })
    }
}

fn metadata_hook(name: &str, python: &str) -> EventHookEntry {
    EventHookEntry::Full(EventHookDefinition {
        name: name.into(),
        source: MiddlewareSourceConfig::Python {
            python: python.into(),
        },
        tools: vec![crate::session_meta_tool::READ_TOOL_NAME.into()],
        fetch: vec![],
        timeout: Duration::from_secs(1),
        on_error: EventHookOnError::Allow,
    })
}

fn attach_metadata_log(config: &GlobalConfig, log: Arc<MetadataLog>) {
    config.write().session.as_mut().unwrap().runtime =
        Some(Arc::new(log as Arc<dyn SessionAppendSink>));
}

#[tokio::test]
async fn event_hook_tool_timeout_allows_next_hook_and_model_turn() {
    let _guard = SINK_LOCK.lock().await;
    let hooks = hooks_config(HookEventKind::UserPromptSubmit, vec![
        metadata_hook("timeout", "add_context('discard')\ntools.harnx_read_session_meta()"),
        metadata_hook("next", "r = tools.harnx_read_session_meta()\nassert r['is_error']\nadd_context('next helper ran')"),
    ]);
    let (config, _) = session_config_with_hooks(hooks);
    let log = Arc::new(MetadataLog {
        hang_first: true,
        ..Default::default()
    });
    attach_metadata_log(&config, log.clone());
    let calls = Arc::new(AtomicUsize::new(0));
    let called = calls.clone();
    let ctx = make_test_context(
        config,
        Arc::new(move |input, _config, abort| {
            let called = called.clone();
            Box::pin(async move {
                assert!(!abort.aborted());
                assert_eq!(input.hook_context(), Some("next helper ran"));
                called.fetch_add(1, Ordering::SeqCst);
                Ok(("done".into(), None, vec![], CompletionTokenUsage::default()))
            })
        }),
        no_op_tool_round(),
    );
    let input = crate::config::input::from_str(&ctx.config, "continue after hook timeout", None);
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        harnx_core::sink::with_agent_event_sink(
            Arc::new(CollectingSink::default()),
            run_agent_loop(&ctx, input),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(matches!(result, LoopResult::Completed));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(log.calls.load(Ordering::SeqCst), 2);
    assert_eq!(log.cancelled.load(Ordering::SeqCst), 1);
    assert!(!ctx.abort_signal.aborted());
    assert!(
        !log.log.0.lock().unwrap().iter().any(|entry| matches!(
            entry,
            SessionLogEntry::ToolCalls { .. } | SessionLogEntry::ToolResults { .. }
        )),
        "hook helper calls must not appear in transcript"
    );
}

#[tokio::test]
async fn event_hook_stop_and_stop_failure_dispatch_with_tool_context() {
    let _guard = SINK_LOCK.lock().await;
    for event in [HookEventKind::Stop, HookEventKind::StopFailure] {
        let hooks = hooks_config(
            event,
            vec![metadata_hook("observe", "tools.harnx_read_session_meta()")],
        );
        let (config, _) = session_config_with_hooks(hooks);
        let log = Arc::new(MetadataLog::default());
        attach_metadata_log(&config, log.clone());
        let ctx = make_test_context(
            config,
            Arc::new(move |_, _, _| {
                Box::pin(async move {
                    if event == HookEventKind::StopFailure {
                        anyhow::bail!("model failed");
                    }
                    Ok(("done".into(), None, vec![], CompletionTokenUsage::default()))
                })
            }),
            no_op_tool_round(),
        );
        let input = crate::config::input::from_str(&ctx.config, "observe turn", None);
        let result = harnx_core::sink::with_agent_event_sink(
            Arc::new(CollectingSink::default()),
            run_agent_loop(&ctx, input),
        )
        .await;
        assert_eq!(result.is_err(), event == HookEventKind::StopFailure);
        assert_eq!(log.calls.load(Ordering::SeqCst), 1, "{event:?}");
    }
}

//! Loop-level lifecycle event-hook behavior: prompt gating and rewriting, Stop
//! resume bounds, and hook-context delivery to the model and the transcript
//! (issue #2198).

use super::*;
use crate::config::session::SessionAppendSink;
use harnx_core::hooks::{
    EventHookDefinition, EventHookEntry, EventHookOnError, EventHooksList, HookEventKind,
    HooksConfig,
};
use harnx_core::message::{Message, MessageContent, MessageContentToolCalls, MessageRole};
use harnx_core::middleware::MiddlewareSourceConfig;
use harnx_core::session::SessionLogEntry;

fn bash_hook(name: &str, bash: &str) -> EventHookEntry {
    EventHookEntry::Full(EventHookDefinition {
        name: name.to_string(),
        source: MiddlewareSourceConfig::Bash {
            bash: bash.to_string(),
        },
        fetch: vec![],
        timeout: std::time::Duration::from_secs(30),
        on_error: EventHookOnError::Allow,
    })
}

// qual:allow(dry, fragment) reason: "Mirrors the integration-tree builder; unit and integration test binaries cannot share helpers without exporting test-only code."
fn hooks_config(event: HookEventKind, entries: Vec<EventHookEntry>) -> HooksConfig {
    let list = EventHooksList { entries };
    let mut config = HooksConfig::default();
    match event {
        HookEventKind::SessionStart => config.session_start = list,
        HookEventKind::UserPromptSubmit => config.user_prompt_submit = list,
        HookEventKind::Stop => config.stop = list,
        HookEventKind::StopFailure => config.stop_failure = list,
    }
    config
}

#[derive(Default)]
struct PromptLog(std::sync::Mutex<Vec<SessionLogEntry>>);

impl SessionAppendSink for PromptLog {
    fn append(&self, entry: &SessionLogEntry) -> Result<u64> {
        let mut entries = self.0.lock().unwrap();
        entries.push(entry.clone());
        Ok(entries.len() as u64)
    }

    fn failure_is_fatal(&self) -> bool {
        true
    }
}

impl PromptLog {
    fn user_texts(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|entry| match entry {
                SessionLogEntry::Message {
                    role: MessageRole::User,
                    content,
                    ..
                } => Some(content.to_text()),
                _ => None,
            })
            .filter(|text| !text.starts_with("[Runtime note]"))
            .collect()
    }
}

fn session_config_with_hooks(hooks: HooksConfig) -> (GlobalConfig, Arc<PromptLog>) {
    let mut config = Config::default();
    config.data.hooks = Some(hooks);
    let mut session = crate::config::session::new(&config, "event-hooks", None).unwrap();
    let log = Arc::new(PromptLog::default());
    session.runtime = Some(Arc::new(log.clone() as Arc<dyn SessionAppendSink>));
    config.session = Some(session);
    (Arc::new(ConfigLock::new(config)), log)
}

fn user_texts_of(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter(|message| message.role.is_user())
        .map(|message| message.content.to_text())
        .filter(|text| !text.starts_with("[Runtime note]"))
        .collect()
}

/// A model call that records the user texts of every request it serves.
fn recording_call_fn(
    seen: Arc<std::sync::Mutex<Vec<Vec<String>>>>,
    reply: &'static str,
) -> AgentCallFn {
    Arc::new(move |input, config, _abort| {
        let seen = Arc::clone(&seen);
        Box::pin(async move {
            let messages = crate::config::input::build_messages(input, config)?;
            seen.lock().unwrap().push(user_texts_of(&messages));
            Ok((
                reply.to_string(),
                None,
                vec![],
                CompletionTokenUsage::default(),
            ))
        })
    })
}

#[tokio::test]
async fn user_prompt_submit_block_rejects_turn_before_persistence() {
    let _guard = SINK_LOCK.lock().await;
    let hooks = hooks_config(
        HookEventKind::UserPromptSubmit,
        vec![bash_hook("gate", "block \"not allowed\"")],
    );
    let (config, log) = session_config_with_hooks(hooks);
    let ctx = make_test_context(
        config,
        Arc::new(|_, _, _| unreachable!("a blocked prompt must not reach the model")),
        no_op_tool_round(),
    );
    let input = crate::config::input::from_str(&ctx.config, "do the thing", None);
    let sink = Arc::new(CollectingSink::default());

    let result = harnx_core::sink::with_agent_event_sink(sink.clone(), run_agent_loop(&ctx, input))
        .await
        .unwrap();

    assert!(matches!(result, LoopResult::Completed));
    // The block happened before input preparation: nothing reached the log.
    assert!(log.0.lock().unwrap().is_empty());
    assert!(sink.events.lock().unwrap().iter().any(|(event, source)| {
        source.is_none()
            && matches!(event, AgentEvent::Notice(NoticeEvent::Error(message)) if message == "not allowed")
    }));
}

#[tokio::test]
async fn user_prompt_submit_rewrite_persists_as_the_user_row() {
    let _guard = SINK_LOCK.lock().await;
    let hooks = hooks_config(
        HookEventKind::UserPromptSubmit,
        vec![bash_hook("rephrase", "rewrite_prompt \"rewritten prompt\"")],
    );
    let (config, log) = session_config_with_hooks(hooks);
    // qual:allow(dry, fragment) reason: "Test bodies share the three-statement recording scaffold (seen sink, context, input) that keeps each assertion readable end to end."
    let seen: Arc<std::sync::Mutex<Vec<Vec<String>>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let ctx = make_test_context(
        config,
        recording_call_fn(Arc::clone(&seen), "done"),
        no_op_tool_round(),
    );
    let input = crate::config::input::from_str(&ctx.config, "do the thing", None);

    let result = harnx_core::sink::with_agent_event_sink(
        Arc::new(CollectingSink::default()),
        run_agent_loop(&ctx, input),
    )
    .await
    .unwrap();

    assert!(matches!(result, LoopResult::Completed));
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        [vec!["rewritten prompt".to_string()]],
        "the model must see the rewritten prompt"
    );
    assert_eq!(
        log.user_texts(),
        vec!["rewritten prompt".to_string()],
        "the durable user row must be the rewritten prompt"
    );
}

#[tokio::test]
async fn stop_hook_resume_respects_max_resume() {
    let _guard = SINK_LOCK.lock().await;
    let hooks = hooks_config(
        HookEventKind::Stop,
        vec![bash_hook("keep-going", "resume \"continue the task\"")],
    );
    let (config, _log) = session_config_with_hooks(hooks);
    let seen: Arc<std::sync::Mutex<Vec<Vec<String>>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut ctx = make_test_context(
        config,
        recording_call_fn(Arc::clone(&seen), "working"),
        no_op_tool_round(),
    );
    ctx.max_resume = Some(1);
    let input = crate::config::input::from_str(&ctx.config, "do the thing", None);

    let result = harnx_core::sink::with_agent_event_sink(
        Arc::new(CollectingSink::default()),
        run_agent_loop(&ctx, input),
    )
    .await
    .unwrap();

    assert!(matches!(result, LoopResult::Completed));
    let requests = seen.lock().unwrap().clone();
    assert_eq!(
        requests.len(),
        2,
        "one resume is allowed by max_resume: {requests:?}"
    );
    assert_eq!(requests[0], vec!["do the thing".to_string()]);
    assert_eq!(
        requests[1],
        vec!["do the thing".to_string(), "continue the task".to_string()],
        "the resume message is appended to the turn's conversation"
    );
}

#[tokio::test]
async fn stop_hook_resume_is_bounded_when_max_resume_is_exhausted() {
    let _guard = SINK_LOCK.lock().await;
    let hooks = hooks_config(
        HookEventKind::Stop,
        vec![bash_hook("keep-going", "resume \"continue the task\"")],
    );
    let (config, _log) = session_config_with_hooks(hooks);
    // qual:allow(dry, fragment) reason: "Test bodies share the three-statement recording scaffold (seen sink, context, input) that keeps each assertion readable end to end."
    let seen: Arc<std::sync::Mutex<Vec<Vec<String>>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    // make_test_context defaults to max_resume: Some(0): no resumes at all.
    let ctx = make_test_context(
        config,
        recording_call_fn(Arc::clone(&seen), "working"),
        no_op_tool_round(),
    );
    let input = crate::config::input::from_str(&ctx.config, "do the thing", None);

    let result = harnx_core::sink::with_agent_event_sink(
        Arc::new(CollectingSink::default()),
        run_agent_loop(&ctx, input),
    )
    .await
    .unwrap();

    assert!(matches!(result, LoopResult::Completed));
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "max_resume exhausted: no further model calls"
    );
}

/// Issue #2198: context queued by hooks must reach the next model call on
/// worker turns (`skip_user_log_append`), as a request message and as a
/// durable transcript row.
#[tokio::test]
async fn hook_context_reaches_model_and_transcript_on_worker_turns() {
    let _guard = SINK_LOCK.lock().await;
    let (config, log) = session_config_with_hooks(HooksConfig::default());
    let seen: Arc<std::sync::Mutex<Vec<Vec<String>>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    // Worker turns always resume existing durable history, and the NATS
    // worker replays the turn's folded user rows into the in-memory session.
    let durable_row = Message::new(
        MessageRole::User,
        MessageContent::Text("worker prompt".into()),
    )
    .with_id("worker-admission".to_string())
    .with_log_seq(1);
    config
        .write()
        .session
        .as_mut()
        .unwrap()
        .messages
        .push(durable_row.clone());
    let mut ctx = make_test_context(
        config,
        recording_call_fn(Arc::clone(&seen), "done"),
        no_op_tool_round(),
    );
    ctx.pending_async_context = Some(Arc::new(tokio::sync::Mutex::new(Some(
        "worker hook context".to_string(),
    ))));
    // `skip_user_log_append` keeps the loop from re-logging the prompt that
    // the worker already made durable.
    let mut input = crate::config::input::from_str(&ctx.config, "worker prompt", None);
    input.durable_user_messages = vec![durable_row];
    input.skip_user_log_append = true;

    let result = harnx_core::sink::with_agent_event_sink(
        Arc::new(CollectingSink::default()),
        run_agent_loop(&ctx, input),
    )
    .await
    .unwrap();

    assert!(matches!(result, LoopResult::Completed));
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        [vec![
            "worker prompt".to_string(),
            "worker hook context".to_string()
        ]],
        "the request must carry the queued hook context"
    );
    assert_eq!(
        log.user_texts(),
        vec!["worker hook context".to_string()],
        "the transcript must gain a durable row even under skip_user_log_append"
    );
}

/// Issue #2198, mid-turn case: context drained into a tool continuation round
/// reaches the next model call and the transcript.
#[tokio::test]
async fn hook_context_reaches_model_mid_turn() {
    let _guard = SINK_LOCK.lock().await;
    let (config, log) = session_config_with_hooks(HooksConfig::default());
    let ctx = make_test_context(
        config,
        Arc::new(|_, _, _| unreachable!("this test drives input preparation directly")),
        no_op_tool_round(),
    );
    let mut input = crate::config::input::from_str(&ctx.config, "", None);
    // A mid-tool-round continuation: the turn's rows are already durable and
    // the request suppresses the folded prompt text.
    input.tool_calls = Some(MessageContentToolCalls::new(
        vec![],
        "working".to_string(),
        None,
    ));
    input.session_input_start = Some(0);
    input.skip_user_log_append = true;
    input.push_hook_context("mid-turn hook context");

    prepare_round_input(&ctx, &mut input, false).await.unwrap();

    let messages = crate::config::input::build_messages(&input, &ctx.config).unwrap();
    assert!(
        user_texts_of(&messages)
            .iter()
            .any(|text| text == "mid-turn hook context"),
        "the mid-turn request must carry the hook context: {messages:?}"
    );
    assert_eq!(
        log.user_texts(),
        vec!["mid-turn hook context".to_string()],
        "the mid-turn continuation must still record the context durably"
    );
}

/// Review N1: a `UserPromptSubmit` rewrite must reach the model on a NATS
/// worker turn. The worker folds its prompt out of admitted durable rows and
/// suppresses re-appending them, so rewriting `Input` alone leaves the model
/// answering the original client text; the rewrite has to land on the durable
/// rows the request is built from, without appending a second user row.
#[tokio::test]
async fn user_prompt_submit_rewrite_reaches_worker_durable_rows() {
    let _guard = SINK_LOCK.lock().await;
    let hooks = hooks_config(
        HookEventKind::UserPromptSubmit,
        vec![bash_hook("rephrase", "rewrite_prompt \"rewritten prompt\"")],
    );
    let (config, log) = session_config_with_hooks(hooks);
    let seen: Arc<std::sync::Mutex<Vec<Vec<String>>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    // The admitted durable row is already in the loaded session, exactly as
    // the NATS worker replays it, and the worker suppresses re-appending the
    // folded prompt text.
    let durable_row = Message::new(
        MessageRole::User,
        MessageContent::Text("worker prompt".into()),
    )
    .with_id("worker-admission".to_string())
    .with_log_seq(1);
    config
        .write()
        .session
        .as_mut()
        .unwrap()
        .messages
        .push(durable_row.clone());
    let ctx = make_test_context(
        config.clone(),
        recording_call_fn(Arc::clone(&seen), "done"),
        no_op_tool_round(),
    );
    let mut input = crate::config::input::from_str(&ctx.config, "worker prompt", None);
    input.durable_user_messages = vec![durable_row];
    input.skip_user_log_append = true;

    let result = harnx_core::sink::with_agent_event_sink(
        Arc::new(CollectingSink::default()),
        run_agent_loop(&ctx, input),
    )
    .await
    .unwrap();

    assert!(matches!(result, LoopResult::Completed));
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        [vec!["rewritten prompt".to_string()]],
        "the model must see the rewritten prompt, not the original durable text"
    );
    assert!(
        log.user_texts().is_empty(),
        "no duplicate durable user row: the original keeps its admission identity"
    );
    let config = config.read();
    let session = config.session.as_ref().expect("session stays attached");
    let session_row = session
        .messages
        .iter()
        .find(|message| message.id.as_deref() == Some("worker-admission"))
        .expect("the admitted row keeps its id");
    assert_eq!(
        session_row.content.to_text(),
        "rewritten prompt",
        "the in-session durable row carries the rewrite for this turn's requests"
    );
}

/// Review F4: a tool-round continuation (approval resume, middleware
/// recovery) re-enters the loop with the round's tool results merged into the
/// input. The submitted prompt already went through the UserPromptSubmit chain
/// when its turn started; the chain must not re-fire after tools executed.
#[tokio::test]
async fn tool_round_continuation_does_not_refire_user_prompt_hooks() {
    let _guard = SINK_LOCK.lock().await;
    let hooks = hooks_config(
        HookEventKind::UserPromptSubmit,
        vec![bash_hook(
            "non-idempotent",
            "rewrite_prompt \"rewritten prompt\"\nadd_context \"hook context\"",
        )],
    );
    let (config, log) = session_config_with_hooks(hooks);
    let seen: Arc<std::sync::Mutex<Vec<Vec<String>>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    // The session looks like it did when the round suspended for approval:
    // the admitted prompt, then the assistant's pending tool-call turn.
    let call = ToolCall::new(
        "noop".to_string(),
        serde_json::json!({}),
        Some("call-1".to_string()),
        None,
    );
    {
        let mut config_guard = config.write();
        let session = config_guard.session.as_mut().unwrap();
        session.messages.push(
            Message::new(
                MessageRole::User,
                MessageContent::Text("original prompt".into()),
            )
            .with_log_seq(1),
        );
        session.messages.push(
            Message::new(
                MessageRole::Tool,
                MessageContent::ToolCalls(MessageContentToolCalls::new(
                    vec![ToolResult::new(call.clone(), serde_json::json!({}))],
                    "calling tool".to_string(),
                    None,
                )),
            )
            .with_log_seq(2),
        );
    }
    let ctx = make_test_context(
        config,
        recording_call_fn(Arc::clone(&seen), "done"),
        no_op_tool_round(),
    );
    // The turn's prompt went through the hook chain before the round
    // suspended; this input is the same one the continuation receives.
    let input = crate::config::input::from_str(&ctx.config, "original prompt", None);
    let round = PendingToolRound {
        seq: 2,
        output: "calling tool".to_string(),
        thought: None,
        calls: vec![call],
    };
    let decisions = vec![ToolApprovalDecision {
        tool_call_id: "call-1".to_string(),
        approved: false,
        reason: Some("denied by user".to_string()),
    }];

    let result = harnx_core::sink::with_agent_event_sink(
        Arc::new(CollectingSink::default()),
        continue_agent_loop_from_tool_round(&ctx, input, round, decisions, Default::default()),
    )
    .await
    .unwrap();

    assert!(matches!(result, LoopResult::Completed));
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        [vec!["original prompt".to_string()]],
        "the continuation request must keep the original prompt: no rewrite, no hook context"
    );
    assert!(
        log.user_texts().is_empty(),
        "the hook chain must not append a rewrite or context row after tools ran"
    );
}

//! Input is history before a model response can be accepted or rejected.
use super::*;

mod retry_fallback;
use crate::config::session::{replay_log_entries_for_external, SessionAppendSink};
use harnx_core::{input::TimingContext, timing_identity::TimingReceipt};
use harnx_core::{
    message::{Message, MessageContent},
    session::SessionLogEntry,
};
use retry_fallback::{
    assert_retry_fallback_requests, execute_retry_and_fallback_calls,
    make_retry_fallback_turn_context, setup_retry_fallback_input,
};
use tokio::sync::Barrier;
use tokio_util::task::AbortOnDropHandle;

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
    fn entries(&self) -> Vec<(usize, SessionLogEntry)> {
        self.0.lock().unwrap().iter().cloned().enumerate().collect()
    }

    fn messages(&self) -> Vec<Message> {
        replay_log_entries_for_external(&self.entries(), "input-persistence", false)
            .unwrap()
            .messages
    }
}

fn session_config() -> (GlobalConfig, Arc<PromptLog>) {
    let mut config = Config::default();
    let mut session = crate::config::session::new(&config, "input-persistence", None).unwrap();
    let log = Arc::new(PromptLog::default());
    session.runtime = Some(Arc::new(log.clone() as Arc<dyn SessionAppendSink>));
    config.session = Some(session);
    (Arc::new(crate::config::ConfigLock::new(config)), log)
}

fn user_texts(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter(|message| {
            message.role.is_user()
                && !harnx_core::timing_identity::is_timing_notice(&message.content.to_text())
        })
        .map(|message| message.content.to_text())
        .collect()
}

fn paused_completion(entered: Arc<Barrier>, release: Arc<Barrier>) -> AgentCallFn {
    Arc::new(move |input, config, _abort| {
        let (entered, release) = (entered.clone(), release.clone());
        Box::pin(async move {
            assert_eq!(
                user_texts(&crate::config::input::build_messages(input, config)?),
                ["cancel me"]
            );
            entered.wait().await;
            release.wait().await;
            Ok((
                "late model output".into(),
                None,
                vec![ToolCall::new(
                    "never_dispatch".into(),
                    json!({}),
                    None,
                    None,
                )],
                Default::default(),
            ))
        })
    })
}

fn assert_cancelled_user_prompt_durable(
    before_cancel: &[Message],
    log: &PromptLog,
    expected_prompt: &str,
) {
    assert_eq!(
        user_texts(before_cancel),
        [expected_prompt],
        "input must be durable before output"
    );
    assert_eq!(user_texts(&log.messages()), [expected_prompt]);
}

fn assert_cancelled_timing_and_session_state(
    before_cancel: &[Message],
    log: &PromptLog,
    config: &GlobalConfig,
    expected_prompt: &str,
) {
    assert_eq!(timing_messages(before_cancel).len(), 1);
    assert_eq!(timing_messages(&log.messages()).len(), 1);
    assert_eq!(
        user_texts(&config.read().session.as_ref().unwrap().messages),
        [expected_prompt]
    );
}

fn assert_cancelled_log_entries_all_user(log: &PromptLog) {
    assert!(
        log.entries().iter().all(|(_, entry)| matches!(
            entry,
            SessionLogEntry::Message {
                role: MessageRole::User,
                ..
            }
        )),
        "late model output must not authorize ToolCalls, results, or a normal end"
    );
}

fn assert_cancelled_user_input_persisted(
    before_cancel: &[Message],
    log: &PromptLog,
    config: &GlobalConfig,
    expected_prompt: &str,
) {
    assert_cancelled_user_prompt_durable(before_cancel, log, expected_prompt);
    assert_cancelled_timing_and_session_state(before_cancel, log, config, expected_prompt);
    assert_cancelled_log_entries_all_user(log);
}

fn assert_durable_timing_counts(durable: &[Message]) {
    assert_eq!(user_texts(durable), ["opening prompt", "queued prompt"]);
    assert_eq!(timing_messages(durable).len(), 1);
}

fn assert_durable_timing_content(durable: &[Message], context: &TimingContext) {
    assert_eq!(timing_messages(durable)[0].content, context.message.content);
    assert_eq!(timing_messages(durable)[0].id, context.message.id);
    assert!(context.message.log_seq.is_some());
}

fn assert_durable_timing_match(durable: &[Message], context: &TimingContext) {
    assert_durable_timing_counts(durable);
    assert_durable_timing_content(durable, context);
}

fn assert_append_failure_uncommitted_state(
    error: &anyhow::Error,
    input: &crate::config::input::Input,
    config: &GlobalConfig,
) {
    assert!(format!("{error:#}").contains("simulated timing append failure"));
    assert!(input.timing_context.is_none());
    assert!(config
        .read()
        .timing_run
        .as_ref()
        .unwrap()
        .state
        .last_receipt
        .is_none());
}

fn assert_append_failure_notices_omitted(
    input: &mut crate::config::input::Input,
    config: &GlobalConfig,
    log: &PromptLog,
) -> Result<()> {
    assert_eq!(timing_messages(&log.messages()).len(), 0);
    assert_eq!(
        timing_messages(&config.read().session.as_ref().unwrap().messages).len(),
        0
    );
    assert_eq!(timing_messages(&built_request(input, config)?).len(), 0);
    Ok(())
}

fn assert_append_failure_state(
    error: &anyhow::Error,
    input: &mut crate::config::input::Input,
    config: &GlobalConfig,
    log: &PromptLog,
) -> Result<()> {
    assert_append_failure_uncommitted_state(error, input, config);
    assert_append_failure_notices_omitted(input, config, log)?;
    Ok(())
}

fn assert_retry_attempt_entries_stable(attempts: &[SessionLogEntry]) -> Result<()> {
    assert_eq!(attempts.len(), 2);
    assert_eq!(
        serde_json::to_value(&attempts[0])?,
        serde_json::to_value(&attempts[1])?,
        "retry keeps original stable entry"
    );
    Ok(())
}

fn assert_retry_timing_message_counts(
    log: &PromptLog,
    input: &mut crate::config::input::Input,
    config: &GlobalConfig,
) -> Result<()> {
    assert_eq!(timing_messages(&log.messages()).len(), 1);
    assert_eq!(timing_messages(&built_request(input, config)?).len(), 1);
    Ok(())
}

fn assert_retry_attempts_and_success(
    attempts: &[SessionLogEntry],
    log: &PromptLog,
    input: &mut crate::config::input::Input,
    config: &GlobalConfig,
) -> Result<()> {
    assert_retry_attempt_entries_stable(attempts)?;
    assert_retry_timing_message_counts(log, input, config)?;
    Ok(())
}

fn assert_preparation_failure_error_and_dispatch(error: &anyhow::Error, calls: &AtomicUsize) {
    assert!(format!("{error:#}").contains("simulated timing append failure"));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

fn assert_preparation_failure_state_cleared(log: &PromptLog, config: &GlobalConfig) {
    assert_eq!(timing_messages(&log.messages()).len(), 0);
    assert!(config
        .read()
        .timing_run
        .as_ref()
        .unwrap()
        .state
        .last_receipt
        .is_none());
}

fn assert_preparation_failure_aborts_dispatch(
    error: &anyhow::Error,
    calls: &AtomicUsize,
    log: &PromptLog,
    config: &GlobalConfig,
) {
    assert_preparation_failure_error_and_dispatch(error, calls);
    assert_preparation_failure_state_cleared(log, config);
}

#[tokio::test]
async fn cancel_during_model_preserves_already_persisted_user_input() -> Result<()> {
    let (config, log) = session_config();
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let call = paused_completion(entered.clone(), release.clone());
    let ctx = make_test_context(config.clone(), call, handoff_on_tool_round());
    let abort = ctx.abort_signal.clone();
    let input = crate::config::input::from_str(&config, "cancel me", None);
    let worker = AbortOnDropHandle::new(tokio::spawn(
        async move { run_agent_loop(&ctx, input).await },
    ));
    tokio::time::timeout(std::time::Duration::from_secs(10), entered.wait()).await?;
    let before_cancel = log.messages();
    abort.set_ctrlc();
    release.wait().await;
    assert!(worker.await?.is_err());

    assert_cancelled_user_input_persisted(&before_cancel, &log, &config, "cancel me");
    Ok(())
}

#[tokio::test]
async fn preparing_input_keeps_request_patches_out_of_history_and_does_not_duplicate() -> Result<()>
{
    let (config, log) = session_config();
    let mut input = crate::config::input::from_str(&config, "original prompt", None);
    input.set_patched_text(Some("retrieved context plus prompt".into()));
    config.write().before_chat_completion(&mut input)?;
    config.write().before_chat_completion(&mut input)?;
    assert_eq!(user_texts(&log.messages()), ["original prompt"]);
    assert_eq!(
        user_texts(&crate::config::input::build_messages(&input, &config)?),
        ["retrieved context plus prompt"]
    );
    let persistence = config.write().prepare_after_chat_completion(
        &SessionSaveRequest::new(&input, "answer", None),
        &[],
        &Default::default(),
    )?;
    persistence.persist().await;
    assert_eq!(log.entries().len(), 2);
    assert_eq!(user_texts(&log.messages()), ["original prompt"]);
    Ok(())
}

#[tokio::test]
async fn injected_prompt_is_history_before_next_model_without_wire_duplication() -> Result<()> {
    let (config, log) = session_config();
    let mut input = crate::config::input::from_str(&config, "opening prompt", None);
    config.write().before_chat_completion(&mut input)?;
    let call = ToolCall::new("tool".into(), json!({}), Some("call".into()), None);
    config
        .write()
        .append_session_tool_calls(&input, "", None, std::slice::from_ref(&call))?;
    let result = ToolResult::new(call, json!({"ok": true}));
    let persistence = config
        .write()
        .prepare_session_tool_results(std::slice::from_ref(&result))?;
    persistence.persist().await;
    input = input.merge_tool_results("".into(), None, vec![result]);
    input.set_injected_user_text("queued prompt".into());
    config.write().before_chat_completion(&mut input)?;
    assert_eq!(
        user_texts(&log.messages()),
        ["opening prompt", "queued prompt"]
    );
    assert_eq!(
        user_texts(&crate::config::input::build_messages(&input, &config)?),
        ["opening prompt", "queued prompt"]
    );
    let persistence = config.write().prepare_after_chat_completion(
        &SessionSaveRequest::new(&input, "answer", None),
        &[],
        &Default::default(),
    )?;
    persistence.persist().await;
    assert_eq!(
        user_texts(&log.messages()),
        ["opening prompt", "queued prompt"]
    );
    Ok(())
}

#[test]
fn prepare_input_preserves_dry_run_detached_and_edit_modes() -> Result<()> {
    for mode in ["dry_run", "detached", "continue", "regenerate"] {
        let (config, log) = session_config();
        let mut input = crate::config::input::from_str(&config, "not a new input", None);
        match mode {
            "dry_run" => config.write().dry_run = true,
            "detached" => input.with_session = false,
            "continue" => input.set_continue_output("previous output"),
            "regenerate" => input.regenerate = true,
            _ => unreachable!(),
        }
        config.write().before_chat_completion(&mut input)?;
        assert!(
            log.entries().is_empty(),
            "{mode} must not append a user row"
        );
        assert!(input.session_input_start.is_none());
    }
    Ok(())
}

#[tokio::test]
async fn prepared_input_can_be_reused_for_continue_and_regenerate() -> Result<()> {
    let (config, log) = session_config();
    let mut input = crate::config::input::from_str(&config, "original prompt", None);
    config.write().before_chat_completion(&mut input)?;
    let persistence = config.write().prepare_after_chat_completion(
        &SessionSaveRequest::new(&input, "answer", None),
        &[],
        &Default::default(),
    )?;
    persistence.persist().await;

    let mut continued = input.clone();
    continued.set_continue_output("answer");
    config.write().before_chat_completion(&mut continued)?;
    let messages = crate::config::input::build_messages(&continued, &config)?;
    assert_eq!(user_texts(&messages), ["original prompt"]);
    assert_eq!(messages.last().unwrap().content.to_text(), "answer");

    input.regenerate = true;
    config.write().before_chat_completion(&mut input)?;
    let messages = crate::config::input::build_messages(&input, &config)?;
    assert_eq!(user_texts(&messages), ["original prompt"]);
    assert!(messages.last().unwrap().role.is_user());
    assert_eq!(
        log.entries().len(),
        2,
        "edit modes reuse input; they don't reappend it"
    );
    Ok(())
}

fn timing_messages(messages: &[Message]) -> Vec<&Message> {
    messages
        .iter()
        .filter(|message| {
            message.role.is_user()
                && harnx_core::timing_identity::is_timing_notice(&message.content.to_text())
        })
        .collect()
}

fn built_request(input: &mut Input, config: &GlobalConfig) -> Result<Vec<Message>> {
    let model = input.agent().model().clone();
    let client = crate::test_utils::MockClient::builder()
        .model(model.clone())
        .build();
    Ok(
        crate::config::input::prepare_completion_data(input, config, &model, false, &client)?
            .messages,
    )
}

fn prepare_at(
    input: &mut Input,
    config: &GlobalConfig,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    config.write().before_chat_completion(input)?;
    crate::config::session_timing::prepare_notice(config, input, now)
}

fn timing_origin() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339("2026-10-14T12:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc)
}

fn prelog_prompt(config: &GlobalConfig, log: &PromptLog, text: &str) -> Message {
    let entry = SessionLogEntry::Message {
        id: Some(format!("prompt-{text}")),
        role: MessageRole::User,
        content: MessageContent::Text(text.into()),
        timestamp: Some(timing_origin()),
        fence_token: None,
    };
    let seq = log.append(&entry).unwrap();
    let message = Message::new(MessageRole::User, MessageContent::Text(text.into()))
        .with_id(format!("prompt-{text}"))
        .with_log_seq(seq as usize);
    config.write().session.as_mut().unwrap().log_entry_count = seq as usize;
    message
}

#[test]
fn timing_initial_is_durable_and_on_wire_including_skip_user_log_mode() -> Result<()> {
    for skip in [false, true] {
        let (config, log) = session_config();
        let mut input = crate::config::input::from_str(&config, "opening prompt", None);
        if skip {
            let message = prelog_prompt(&config, &log, "opening prompt");
            config
                .write()
                .session
                .as_mut()
                .unwrap()
                .messages
                .push(message);
            prelog_prompt(&config, &log, "queued prompt");
        }
        input.skip_user_log_append = skip;
        input.set_injected_user_text("queued prompt".into());
        let origin = timing_origin();
        crate::config::session_timing::start_run(&config, origin);
        prepare_at(&mut input, &config, origin)?;
        assert_eq!(input.injected_user_text(), Some("queued prompt"));
        let messages = built_request(&mut input, &config)?;
        assert_eq!(user_texts(&messages), ["opening prompt", "queued prompt"]);
        let context = input.timing_context.as_ref().unwrap();
        assert_eq!(context.receipt.origin, origin);
        assert_eq!(timing_messages(&messages).len(), 1);
        assert_eq!(messages.last().unwrap().content, context.message.content);
        let durable = log.messages();
        assert_durable_timing_match(&durable, context);
        let count = log.entries().len();
        prepare_at(&mut input, &config, origin + chrono::Duration::seconds(1))?;
        assert_eq!(
            log.entries().len(),
            count,
            "preparation retry cannot reappend"
        );
        assert_eq!(
            timing_messages(&built_request(&mut input, &config)?).len(),
            1
        );
    }
    Ok(())
}

fn assert_tool_call_result_structure(message: &Message, expected_call_id: &str) {
    assert_eq!(message.role, MessageRole::Tool);
    let MessageContent::ToolCalls(tool_context) = &message.content else {
        unreachable!()
    };
    assert_eq!(tool_context.tool_results.len(), 1);
    assert_eq!(
        tool_context.tool_results[0].call.id.as_deref(),
        Some(expected_call_id)
    );
}

struct ExpectedToolSuffix<'a> {
    call_id: &'a str,
    text: &'a str,
    queued: &'a str,
    timing_content: &'a MessageContent,
}

fn assert_tool_trailing_queued_and_timing(
    messages: &[Message],
    tool_index: usize,
    expected: &ExpectedToolSuffix<'_>,
) {
    let MessageContent::ToolCalls(tool_context) = &messages[tool_index].content else {
        unreachable!()
    };
    assert_eq!(tool_context.text, expected.text);
    assert_eq!(messages[tool_index + 1].content.to_text(), expected.queued);
    assert_eq!(messages[tool_index + 2].content, *expected.timing_content);
}

fn assert_tool_calls_followed_by_queued_and_timing(
    messages: &[Message],
    expected: &ExpectedToolSuffix<'_>,
) {
    let tool_index = messages
        .iter()
        .position(|message| matches!(message.content, MessageContent::ToolCalls(_)))
        .unwrap();
    assert_tool_call_result_structure(&messages[tool_index], expected.call_id);
    assert_tool_trailing_queued_and_timing(messages, tool_index, expected);
}

async fn assert_periodic_suffix_with_skip(skip: bool) -> Result<()> {
    let (config, log) = session_config();
    let origin = timing_origin();
    crate::config::session_timing::start_run(&config, origin);
    let mut input = crate::config::input::from_str(&config, "opening prompt", None);
    prepare_at(&mut input, &config, origin)?;
    let initial = input.timing_context.take().unwrap();
    let call = ToolCall::new("noop".into(), json!({}), Some("call-1".into()), None);
    config.write().append_session_tool_calls(
        &input,
        "working",
        None,
        std::slice::from_ref(&call),
    )?;
    let results = vec![ToolResult::new(call, json!({"ok": true}))];
    let persistence = config.write().prepare_session_tool_results(&results)?;
    persistence.persist().await;
    input = input.merge_tool_results("working".into(), None, results);
    input.skip_user_log_append = skip;
    if skip {
        prelog_prompt(&config, &log, "queued prompt");
    }
    input.set_injected_user_text("queued prompt".into());
    prepare_at(&mut input, &config, origin + chrono::Duration::seconds(900))?;
    let periodic = input.timing_context.take().unwrap();
    assert_eq!(periodic.receipt.origin, initial.receipt.origin);
    assert_eq!(
        periodic.receipt.invocation_id,
        initial.receipt.invocation_id
    );
    assert_eq!(periodic.receipt.highest_elapsed_bucket, 1);
    input.timing_context = Some(periodic.clone());
    let messages = built_request(&mut input, &config)?;
    assert_eq!(timing_messages(&messages).len(), 2);
    assert_tool_calls_followed_by_queued_and_timing(
        &messages,
        &ExpectedToolSuffix {
            call_id: "call-1",
            text: "working",
            queued: "queued prompt",
            timing_content: &periodic.message.content,
        },
    );
    assert_eq!(timing_messages(&log.messages()).len(), 2);
    assert_next_round_without_new_timing_receipt(input, &config, &log, origin).await
}

async fn assert_next_round_without_new_timing_receipt(
    mut input: crate::config::input::Input,
    config: &GlobalConfig,
    log: &PromptLog,
    origin: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    input.injected_user_text = None;
    input.timing_context = None;
    let call = ToolCall::new("noop".into(), json!({}), Some("call-2".into()), None);
    config.write().append_session_tool_calls(
        &input,
        "more work",
        None,
        std::slice::from_ref(&call),
    )?;
    let results = vec![ToolResult::new(call, json!({"ok": true}))];
    let persistence = config.write().prepare_session_tool_results(&results)?;
    persistence.persist().await;
    input = input.merge_tool_results("more work".into(), None, results);
    let count = log.entries().len();
    prepare_at(&mut input, config, origin + chrono::Duration::seconds(901))?;
    assert!(
        input.timing_context.is_none(),
        "no new receipt on the next round"
    );
    assert_eq!(log.entries().len(), count);
    let messages = built_request(&mut input, config)?;
    assert_eq!(user_texts(&messages), ["opening prompt", "queued prompt"]);
    assert_eq!(
        timing_messages(&messages).len(),
        2,
        "history isn't replayed as a new suffix"
    );
    Ok(())
}

#[tokio::test]
async fn timing_periodic_suffix_follows_complete_tools_and_concurrent_input_once() -> Result<()> {
    for skip in [false, true] {
        assert_periodic_suffix_with_skip(skip).await?;
    }
    Ok(())
}

#[test]
fn timing_receipt_already_in_history_is_not_appended_or_sent_twice() -> Result<()> {
    let (config, log) = session_config();
    let origin = timing_origin();
    let mut input = crate::config::input::from_str(&config, "opening prompt", None);
    crate::config::session_timing::start_run(&config, origin);
    prepare_at(&mut input, &config, origin)?;
    let context = input.timing_context.take().unwrap();
    config
        .write()
        .timing_run
        .as_mut()
        .unwrap()
        .state
        .last_receipt = None;
    input.session_input_start = None;
    input.skip_user_log_append = true;
    let count = log.entries().len();
    prepare_at(&mut input, &config, origin)?;
    assert_eq!(
        log.entries().len(),
        count,
        "existing receipt prevents a durable duplicate"
    );
    assert_eq!(
        timing_messages(&built_request(&mut input, &config)?).len(),
        1
    );
    assert_eq!(
        input.timing_context.as_ref().unwrap().receipt,
        context.receipt
    );
    // Continue/regenerate requests use full history rather than the frozen prefix.
    for regenerate in [false, true] {
        let mut edit = input.clone();
        if regenerate {
            edit.regenerate = true;
        } else {
            edit.set_continue_output("answer");
        }
        assert_eq!(
            timing_messages(&built_request(&mut edit, &config)?).len(),
            1
        );
    }
    Ok(())
}

struct TimingAppendProbe {
    config: std::sync::Weak<crate::config::ConfigLock>,
    log: Arc<PromptLog>,
    fail: std::sync::atomic::AtomicBool,
    attempts: std::sync::Mutex<Vec<SessionLogEntry>>,
}

impl SessionAppendSink for TimingAppendProbe {
    fn append(&self, entry: &SessionLogEntry) -> Result<u64> {
        if let SessionLogEntry::Message { content, .. } = entry {
            if harnx_core::timing_identity::is_timing_notice(&content.to_text()) {
                let config = self.config.upgrade().unwrap();
                assert!(
                    config.try_write().is_some(),
                    "timing append must release config guard"
                );
                self.attempts.lock().unwrap().push(entry.clone());
                if self.fail.swap(false, Ordering::SeqCst) {
                    anyhow::bail!("simulated timing append failure");
                }
            }
        }
        self.log.append(entry)
    }

    fn failure_is_fatal(&self) -> bool {
        true
    }
}

fn failing_timing_sink(config: &GlobalConfig, log: Arc<PromptLog>) -> Arc<TimingAppendProbe> {
    let probe = Arc::new(TimingAppendProbe {
        config: Arc::downgrade(config),
        log,
        fail: std::sync::atomic::AtomicBool::new(true),
        attempts: Default::default(),
    });
    config.write().session.as_mut().unwrap().runtime =
        Some(Arc::new(probe.clone() as Arc<dyn SessionAppendSink>));
    probe
}

#[test]
fn timing_append_failure_keeps_history_progress_and_suffix_uncommitted() -> Result<()> {
    let (config, log) = session_config();
    let probe = failing_timing_sink(&config, log.clone());
    let origin = timing_origin();
    crate::config::session_timing::start_run(&config, origin);
    let mut input = crate::config::input::from_str(&config, "opening prompt", None);
    let error = prepare_at(&mut input, &config, origin).unwrap_err();
    assert_append_failure_state(&error, &mut input, &config, &log)?;
    prepare_at(&mut input, &config, origin + chrono::Duration::seconds(900))?;
    let attempts = probe.attempts.lock().unwrap();
    assert_retry_attempts_and_success(&attempts, &log, &mut input, &config)?;
    Ok(())
}

fn no_tools_callback() -> OnToolRoundFn {
    Arc::new(|_, _| Box::pin(async { Ok(()) }))
}

#[tokio::test]
async fn timing_append_failure_fails_shared_preparation_before_model_dispatch() -> Result<()> {
    let (config, log) = session_config();
    failing_timing_sink(&config, log.clone());
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let call: AgentCallFn = Arc::new(move |_, _, _| {
        count.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(("unexpected".into(), None, vec![], Default::default())) })
    });
    let ctx = make_test_context(config.clone(), call, no_tools_callback());
    let input = crate::config::input::from_str(&config, "opening prompt", None);
    let error = run_agent_loop(&ctx, input)
        .await
        .err()
        .expect("preparation must fail");
    assert_preparation_failure_aborts_dispatch(&error, &calls, &log, &config);
    Ok(())
}

use crate::nats_session_metadata::{
    InvocationIdentity, RunIdentity, RunLimitsPolicySource, RunLimitsRecord,
};

fn assert_admission_frozen_and_fork_dropped(
    config: &GlobalConfig,
    origin: chrono::DateTime<chrono::Utc>,
    admitted: &RunLimitsRecord,
) -> Result<()> {
    crate::config::session_timing::start_run(config, origin + chrono::Duration::seconds(100));
    let mut input = crate::config::input::from_str(config, "opening prompt", None);
    prepare_at(&mut input, config, origin + chrono::Duration::seconds(100))?;
    assert_eq!(
        input.timing_context.as_ref().unwrap().receipt.origin,
        origin
    );
    config.write().run_context.as_mut().unwrap().deadline = None;
    crate::config::session_timing::start_run(config, origin + chrono::Duration::seconds(200));
    assert_eq!(
        config.read().timing_run.as_ref().unwrap().limits,
        Some(admitted.clone())
    );
    assert!(config
        .read()
        .timing_run
        .as_ref()
        .unwrap()
        .state
        .last_receipt
        .is_some());
    assert!(config.read().fork_session_scope().timing_run.is_none());
    Ok(())
}

fn assert_sink_or_invocation_change_resets_timing(
    config: &GlobalConfig,
    origin: chrono::DateTime<chrono::Utc>,
) {
    let prior_key = config.read().timing_run.as_ref().unwrap().state.key.clone();
    config.write().session.as_mut().unwrap().runtime = Some(Arc::new(
        Arc::new(PromptLog::default()) as Arc<dyn SessionAppendSink>,
    ));
    crate::config::session_timing::start_run(config, origin + chrono::Duration::seconds(250));
    assert_ne!(
        config.read().timing_run.as_ref().unwrap().state.key,
        prior_key
    );
    assert!(config
        .read()
        .timing_run
        .as_ref()
        .unwrap()
        .state
        .last_receipt
        .is_none());
    config.write().run_context.as_mut().unwrap().invocation_id = InvocationIdentity::new();
    crate::config::session_timing::start_run(config, origin + chrono::Duration::seconds(300));
    assert!(config
        .read()
        .timing_run
        .as_ref()
        .unwrap()
        .state
        .last_receipt
        .is_none());
}

fn assert_unmanaged_run_resets_origin(
    config: &GlobalConfig,
    origin: chrono::DateTime<chrono::Utc>,
) {
    config.write().run_context = None;
    crate::config::session_timing::start_run(config, origin);
    let key = config.read().timing_run.as_ref().unwrap().state.key.clone();
    crate::config::session_timing::start_run(config, origin + chrono::Duration::seconds(900));
    let guard = config.read();
    let run = guard.timing_run.as_ref().unwrap();
    assert_ne!(key, run.state.key);
    assert_eq!(
        run.state.fallback_origin,
        Some(origin + chrono::Duration::seconds(900))
    );
}

#[test]
fn timing_lifecycle_freezes_admission_resets_new_runs_and_drops_fork_cache() -> Result<()> {
    let (config, _) = session_config();
    let origin = timing_origin();
    let admitted = RunLimitsRecord {
        run_id: RunIdentity::new(),
        invocation_id: InvocationIdentity::new(),
        admitted_at: origin,
        deadline: Some(origin + chrono::Duration::seconds(3600)),
        policy_source: RunLimitsPolicySource::GlobalDefault,
        parent_invocation: None,
    };
    config.write().run_context = Some(admitted.clone());
    assert_admission_frozen_and_fork_dropped(&config, origin, &admitted)?;
    assert_sink_or_invocation_change_resets_timing(&config, origin);
    assert_unmanaged_run_resets_origin(&config, origin);
    Ok(())
}

fn make_timing_loop_recording_call_fn(
    requests: Arc<std::sync::Mutex<Vec<Vec<Message>>>>,
) -> AgentCallFn {
    Arc::new(move |input, config, _| {
        let requests = requests.clone();
        Box::pin(async move {
            let messages = built_request(input, config)?;
            let mut req = requests.lock().unwrap();
            let round = req.len();
            req.push(messages);
            let calls = if round < 2 {
                vec![ToolCall::new(
                    "noop".into(),
                    json!({}),
                    Some(format!("call-{round}")),
                    None,
                )]
            } else {
                vec![]
            };
            Ok((format!("round {round}"), None, calls, Default::default()))
        })
    })
}

fn make_staged_timing_tool_callback(
    config: GlobalConfig,
    entered: Arc<Barrier>,
    release: Arc<Barrier>,
    rounds: Arc<AtomicUsize>,
) -> OnToolRoundFn {
    Arc::new(move |input, _| {
        let config = config.clone();
        let entered = entered.clone();
        let release = release.clone();
        let rounds = rounds.clone();
        Box::pin(async move {
            assert!(
                input.timing_context.is_none(),
                "successful model round clears suffix"
            );
            assert!(input.injected_user_text.is_none());
            let round = rounds.fetch_add(1, Ordering::SeqCst);
            if round == 0 {
                entered.wait().await;
                release.wait().await;
                input.set_injected_user_text("queued prompt".into());
            }
            let origin = config
                .read()
                .timing_run
                .as_ref()
                .unwrap()
                .state
                .fallback_origin
                .unwrap();
            config.write().timing_run.as_mut().unwrap().evaluation_at =
                Some(origin + chrono::Duration::seconds(900 + round as i64));
            Ok(())
        })
    })
}

fn assert_loop_wire_timing_notices(requests: &[Vec<Message>]) {
    assert_eq!(
        requests.len(),
        3,
        "timing must not create extra model calls after done"
    );
    assert_eq!(timing_messages(&requests[0]).len(), 1);
    assert_eq!(timing_messages(&requests[1]).len(), 2);
}

fn assert_loop_wire_user_texts(requests: &[Vec<Message>]) {
    assert_eq!(timing_messages(&requests[2]).len(), 2);
    assert_eq!(user_texts(&requests[0]), ["opening prompt"]);
    for messages in &requests[1..] {
        assert_eq!(user_texts(messages), ["opening prompt", "queued prompt"]);
    }
}

fn assert_loop_durable_log_state(log: &PromptLog) {
    assert_eq!(timing_messages(&log.messages()).len(), 2);
    assert_eq!(
        user_texts(&log.messages()),
        ["opening prompt", "queued prompt"]
    );
}

fn assert_loop_timing_requests(requests: &[Vec<Message>], log: &PromptLog) {
    assert_loop_wire_timing_notices(requests);
    assert_loop_wire_user_texts(requests);
    assert_loop_durable_log_state(log);
}

#[tokio::test(flavor = "multi_thread")]
async fn timing_loop_sends_initial_and_periodic_once_without_replaying_concurrent_input(
) -> Result<()> {
    let (config, log) = session_config();
    let requests = Arc::new(std::sync::Mutex::new(Vec::<Vec<Message>>::new()));
    let call = make_timing_loop_recording_call_fn(requests.clone());
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let rounds = Arc::new(AtomicUsize::new(0));
    let on_tool =
        make_staged_timing_tool_callback(config.clone(), entered.clone(), release.clone(), rounds);
    let ctx = make_test_context(config.clone(), call, on_tool);
    let input = crate::config::input::from_str(&config, "opening prompt", None);
    let worker = AbortOnDropHandle::new(tokio::spawn(
        async move { run_agent_loop(&ctx, input).await },
    ));
    tokio::time::timeout(std::time::Duration::from_secs(10), entered.wait()).await?;
    // No timer dispatches notes or model calls while a tool round is blocked.
    assert_eq!(requests.lock().unwrap().len(), 1);
    assert_eq!(timing_messages(&log.messages()).len(), 1);
    release.wait().await;
    worker.await??;
    let reqs = requests.lock().unwrap();
    assert_loop_timing_requests(&reqs, &log);
    Ok(())
}

#[tokio::test]
async fn timing_expired_run_makes_no_model_calls_or_timing_appends() -> Result<()> {
    use crate::nats_session_metadata::{
        InvocationIdentity, RunIdentity, RunLimitsPolicySource, RunLimitsRecord,
    };
    let (config, log) = session_config();
    let deadline = chrono::Utc::now() - chrono::Duration::seconds(60);
    config.write().run_context = Some(RunLimitsRecord {
        run_id: RunIdentity::new(),
        invocation_id: InvocationIdentity::new(),
        admitted_at: deadline - chrono::Duration::seconds(3600),
        deadline: Some(deadline),
        policy_source: RunLimitsPolicySource::GlobalDefault,
        parent_invocation: None,
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let call: AgentCallFn = Arc::new(move |_, _, _| {
        count.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(("unexpected".into(), None, vec![], Default::default())) })
    });
    let ctx = make_test_context(config.clone(), call, no_tools_callback());
    let input = crate::config::input::from_str(&config, "opening prompt", None);
    let error = run_agent_loop(&ctx, input)
        .await
        .err()
        .expect("preparation must fail");
    assert!(error
        .downcast_ref::<crate::nats_session_metadata::run_limits::DeadlineExpired>()
        .is_some());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(timing_messages(&log.messages()).len(), 0);
    Ok(())
}

#[tokio::test]
async fn timing_suffix_survives_transport_retries_and_fallback_models() -> Result<()> {
    let (config, log) = session_config();
    let origin = timing_origin();
    let (mut input, receipt) = setup_retry_fallback_input(&config, origin)?;
    let ctx = make_retry_fallback_turn_context();
    let requests = execute_retry_and_fallback_calls(&mut input, &ctx, &config).await?;
    assert_retry_fallback_requests(&requests, &receipt, &input, &log);
    Ok(())
}

#[test]
fn timing_dry_run_and_detached_input_do_not_write_notices() -> Result<()> {
    for dry_run in [false, true] {
        let (config, log) = session_config();
        crate::config::session_timing::start_run(&config, timing_origin());
        let mut input = crate::config::input::from_str(&config, "opening prompt", None);
        if dry_run {
            config.write().dry_run = true;
        } else {
            input.with_session = false;
        }
        prepare_at(&mut input, &config, timing_origin())?;
        assert!(input.timing_context.is_none());
        assert!(log.entries().is_empty());
    }
    Ok(())
}

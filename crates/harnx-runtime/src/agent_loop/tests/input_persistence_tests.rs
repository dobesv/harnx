//! Input is history before a model response can be accepted or rejected.
use super::*;
use crate::config::session::{replay_log_entries_for_external, SessionAppendSink};
use harnx_core::{message::Message, session::SessionLogEntry};
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
        replay_log_entries_for_external(&self.entries(), "input-persistence")
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
    (Arc::new(RwLock::new(config)), log)
}

fn user_texts(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter(|message| message.role.is_user())
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

    assert_eq!(
        user_texts(&before_cancel),
        ["cancel me"],
        "input must be durable before output"
    );
    assert_eq!(user_texts(&log.messages()), ["cancel me"]);
    assert_eq!(
        user_texts(&config.read().session.as_ref().unwrap().messages),
        ["cancel me"]
    );
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

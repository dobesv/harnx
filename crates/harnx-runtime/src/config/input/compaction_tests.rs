use super::*;
use crate::client::TestStateGuard;
use crate::config::{session, session_persistence::SessionAppendSink, ConfigLock};
use crate::test_utils::{MockClient, MockTurnBuilder};
use harnx_core::session::SessionLogEntry;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct CaptureLog(Mutex<Vec<(usize, SessionLogEntry)>>);

impl SessionAppendSink for CaptureLog {
    fn append(&self, entry: &SessionLogEntry) -> Result<u64> {
        let mut entries = self.0.lock().unwrap();
        let seq = entries.len() + 1;
        entries.push((seq, entry.clone()));
        Ok(seq as u64)
    }
}

fn entry(role: MessageRole, text: &str) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: Some(format!("{role:?}:{text}")),
        role,
        content: MessageContent::Text(text.into()),
        timestamp: None,
        fence_token: None,
    }
}

fn setup(entries: &[SessionLogEntry], prompt: &str) -> (GlobalConfig, Arc<CaptureLog>) {
    let mut config = Config {
        data: ConfigData {
            stream: false,
            ..Default::default()
        },
        ..Default::default()
    };
    let log = Arc::new(CaptureLog::default());
    for e in entries {
        log.append(e).unwrap();
    }
    let mut base = session::new(&config, "continuity", None).unwrap();
    base.agent_instructions = prompt.into();
    let mut sess =
        session::replay_nats_entries_into_session(&log.0.lock().unwrap(), "continuity", base)
            .unwrap();
    sess.runtime = Some(Arc::new(log.clone() as Arc<dyn SessionAppendSink>));
    config.session = Some(sess);
    (Arc::new(ConfigLock::new(config)), log)
}

fn reload(config: &GlobalConfig, log: &CaptureLog) {
    let base = {
        let guard = config.read();
        let previous = guard.session.as_ref().unwrap();
        let mut base = session::new(&guard, "continuity", None).unwrap();
        base.id = previous.id.clone();
        base.agent_instructions = previous.agent_instructions.clone();
        base.runtime = previous.runtime.clone();
        base
    };
    let loaded =
        session::replay_nats_entries_into_session(&log.0.lock().unwrap(), "continuity", base)
            .unwrap();
    config.write().session = Some(loaded);
}

fn request(config: &GlobalConfig, client: &MockClient, input: &mut Input) -> Vec<Message> {
    prepare_completion_data(input, config, client.model(), false, client)
        .unwrap()
        .messages
}

fn view(messages: &[Message]) -> Vec<(MessageRole, serde_json::Value)> {
    messages
        .iter()
        .map(|m| (m.role, serde_json::to_value(&m.content).unwrap()))
        .collect()
}

fn summary_message(summary: &str) -> Message {
    Message::new(
        MessageRole::User,
        MessageContent::Text(format!(
            "{}Earlier conversation summary:\n\n{summary}",
            session::RUNTIME_NOTE_PREFIX
        )),
    )
}

fn tool_suffix() -> Vec<SessionLogEntry> {
    vec![
        entry(MessageRole::User, "remove hex"),
        SessionLogEntry::ToolCalls {
            text: "editing".into(),
            thought: None,
            calls: vec![crate::tool::ToolCall::new(
                "edit".into(),
                json!({}),
                Some("edit-1".into()),
                None,
            )],
            timestamp: None,
            fence_token: None,
        },
        SessionLogEntry::ToolResults {
            results: vec![harnx_core::session::ToolOutput {
                id: Some("edit-1".into()),
                name: "edit".into(),
                output: json!({"ok": true}),
                markdown: None,
                content: vec![],
                switch_agent: None,
            }],
            timestamp: None,
        },
        entry(MessageRole::Assistant, "hex removed and verified"),
    ]
}

fn assert_compacted_request(suffix: Vec<SessionLogEntry>, prompt: &str) {
    let mut entries = vec![
        entry(MessageRole::User, "archived question"),
        entry(MessageRole::Assistant, "archived answer"),
    ];
    entries.extend(suffix);
    let (config, log) = setup(&entries, prompt);
    let live_suffix = config.read().session.as_ref().unwrap().messages[2..].to_vec();
    let id = config.read().session.as_ref().unwrap().id.clone();
    assert!(Config::apply_compaction_summary(
        &config,
        &id,
        "completed prior work".into(),
        2
    ));
    let client = MockClient::builder().build();
    let mut expected = vec![];
    if !prompt.is_empty() {
        expected.push(Message::new(
            MessageRole::System,
            MessageContent::Text(prompt.into()),
        ));
    }
    expected.push(summary_message("completed prior work"));
    expected.extend(live_suffix);
    expected.push(Message::new(
        MessageRole::User,
        MessageContent::Text("next request".into()),
    ));
    for reloaded in [false, true] {
        if reloaded {
            reload(&config, &log);
        }
        let mut input = from_str(&config, "next request", None);
        assert_eq!(
            view(&build_messages(&input, &config).unwrap()),
            view(&expected)
        );
        assert_eq!(
            view(&request(&config, &client, &mut input)),
            view(&expected)
        );
    }
    assert_eq!(
        config.read().session.as_ref().unwrap().messages.len(),
        expected.len() - 2 - usize::from(!prompt.is_empty())
    );
    assert!(!log.0.lock().unwrap().iter().any(|(_, e)| matches!(e,
        SessionLogEntry::Message { content: MessageContent::Text(t), .. } if t.contains(session::RUNTIME_NOTE_PREFIX))));
}

#[test]
fn prepare_completion_data_preserves_compaction_context_before_and_after_reload() {
    let suffixes = vec![
        vec![],
        vec![entry(MessageRole::User, "single live user")],
        vec![
            entry(MessageRole::User, "first retained"),
            entry(MessageRole::Assistant, "first answer"),
            entry(MessageRole::User, "second retained"),
            entry(MessageRole::Assistant, "second answer"),
        ],
        tool_suffix(),
        tool_suffix()[..3].to_vec(),
    ];
    for prompt in ["", "fresh agent instructions"] {
        for suffix in &suffixes {
            assert_compacted_request(suffix.clone(), prompt);
        }
    }
}

#[test]
fn prepare_completion_data_keeps_logged_user_once_with_queued_and_transient_input() {
    let entries = vec![
        entry(MessageRole::User, "archived"),
        entry(MessageRole::Assistant, "done"),
        entry(MessageRole::User, "live"),
    ];
    let (config, log) = setup(&entries, "");
    let id = config.read().session.as_ref().unwrap().id.clone();
    assert!(Config::apply_compaction_summary(
        &config,
        &id,
        "done work".into(),
        2
    ));
    let client = MockClient::builder().build();
    for reloaded in [false, true] {
        if reloaded {
            reload(&config, &log);
        }
        let mut input = from_str(&config, "live", None);
        input.skip_user_log_append = true;
        input.injected_user_text = Some("queued".into());
        input.transient_note = Some("retry note".into());
        let messages = request(&config, &client, &mut input);
        assert_eq!(
            messages
                .iter()
                .map(|m| m.content.to_text())
                .collect::<Vec<_>>(),
            vec![
                summary_message("done work").content.to_text(),
                "live".into(),
                "queued".into(),
                "retry note".into()
            ]
        );
    }
}

#[test]
fn prepare_completion_data_includes_summary_in_empty_and_edit_mode_history() {
    let (config, _) = setup(&[], "");
    config.write().session.as_mut().unwrap().compaction_summary = Some("completed work".into());
    let client = MockClient::builder().build();
    let mut input = from_str(&config, "", None);
    input.continue_output = Some("partial reply".into());
    assert_eq!(
        view(&request(&config, &client, &mut input)),
        view(&[summary_message("completed work")])
    );
    input.continue_output = None;
    input.regenerate = true;
    assert_eq!(
        view(&request(&config, &client, &mut input)),
        view(&[summary_message("completed work")])
    );
    input.regenerate = false;
    input.inject_system_prompt = false;
    assert_eq!(
        request(&config, &client, &mut input)[0].content.to_text(),
        summary_message("completed work").content.to_text()
    );
    config.write().session.as_mut().unwrap().compaction_summary = Some(String::new());
    assert_eq!(request(&config, &client, &mut input).len(), 1);
}

#[tokio::test]
async fn compact_session_second_summary_incorporates_prior_summary_after_reload() {
    let entries = vec![
        entry(MessageRole::User, "old question"),
        entry(MessageRole::Assistant, "old answer"),
        entry(MessageRole::User, "recent question"),
        entry(MessageRole::Assistant, "recent answer"),
    ];
    let (config, log) = setup(&entries, "");
    let client = Arc::new(
        MockClient::builder()
            .add_turn(
                MockTurnBuilder::new()
                    .add_text_chunk("old work completed")
                    .build(),
            )
            .add_turn(
                MockTurnBuilder::new()
                    .add_text_chunk("old work completed; recent work completed")
                    .build(),
            )
            .add_turn(MockTurnBuilder::new().add_text_chunk("next answer").build())
            .build(),
    );
    let _guard = TestStateGuard::new(Some(client.clone())).await;
    Config::compact_session(&config).await.unwrap();
    let first_summary = stored_summary(&config);
    log.append(&entry(MessageRole::User, "new question"))
        .unwrap();
    log.append(&entry(MessageRole::Assistant, "new answer"))
        .unwrap();
    reload(&config, &log);
    Config::compact_session(&config).await.unwrap();
    assert_cumulative_requests(&client, &first_summary);
    let summary = stored_summary(&config);
    let mut input = from_str(&config, "next request", None);
    fetch_chat_text(&mut input, &config).await.unwrap();
    let sent = view(&client.conversation_history().conversation_history[2].messages);
    assert_eq!(
        sent,
        view(&[
            summary_message(&summary),
            Message::new(
                MessageRole::User,
                MessageContent::Text("new question".into())
            ),
            Message::new(
                MessageRole::Assistant,
                MessageContent::Text("new answer".into())
            ),
            Message::new(
                MessageRole::User,
                MessageContent::Text("next request".into())
            )
        ])
    );
    reload(&config, &log);
    assert_eq!(view(&request(&config, &client, &mut input)), sent);
    assert_eq!(client.remaining_turns(), 0);
}

fn assert_cumulative_requests(client: &MockClient, first_summary: &str) {
    let history = client.conversation_history();
    assert_eq!(history.conversation_history.len(), 2);
    let first = &history.conversation_history[0].messages;
    assert_eq!(first.len(), 2);
    assert!(!first[1].content.to_text().contains("recent question"));
    let second = &history.conversation_history[1].messages;
    assert_eq!(second.len(), 2);
    let transcript = second[1].content.to_text();
    let prior = format!(
        "{}Earlier compaction summary:\n{}\n\n---\n\n",
        session::RUNTIME_NOTE_PREFIX,
        first_summary
    );
    assert!(transcript.starts_with(&prior), "{transcript}");
    assert_eq!(transcript.matches(first_summary).count(), 1);
    assert!(transcript[prior.len()..].contains("recent question"));
    assert!(transcript[prior.len()..].contains("recent answer"));
    assert!(!transcript[prior.len()..].contains("new question"));
}

fn stored_summary(config: &GlobalConfig) -> String {
    config
        .read()
        .session
        .as_ref()
        .unwrap()
        .compaction_summary
        .clone()
        .unwrap()
}

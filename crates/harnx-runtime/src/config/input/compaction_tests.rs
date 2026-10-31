use super::*;
use crate::client::TestStateGuard;
use crate::config::{session, session_persistence::SessionAppendSink, ConfigLock};
use crate::test_utils::{MockClient, MockTurnBuilder};
use harnx_core::session::SessionLogEntry;
use serde_json::Value;
use std::sync::{Arc, Mutex};

#[path = "compaction_tests/budget.rs"]
mod budget;

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
    let mut sess = session::replay_nats_entries_into_session(
        &log.0.lock().unwrap(),
        "continuity",
        base,
        false,
    )
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
    let loaded = session::replay_nats_entries_into_session(
        &log.0.lock().unwrap(),
        "continuity",
        base,
        false,
    )
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
    let keep_plan = crate::config::compaction::KeepPlan {
        prompt_idx: None,
        tail_from: 2,
    };
    assert!(Config::apply_compaction_summary(
        &config,
        &id,
        "completed prior work".into(),
        keep_plan
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
    let keep_plan = crate::config::compaction::KeepPlan {
        prompt_idx: None,
        tail_from: 2,
    };
    assert!(Config::apply_compaction_summary(
        &config,
        &id,
        "done work".into(),
        keep_plan
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

fn session_with_large_middle() -> (GlobalConfig, Arc<CaptureLog>) {
    session_with_middle_output(json!({"text": "word ".repeat(10000)}))
}

fn session_with_middle_output(output: Value) -> (GlobalConfig, Arc<CaptureLog>) {
    let suffix = tool_suffix();
    let mut dropped = suffix[1..3].to_vec();
    if let SessionLogEntry::ToolCalls { calls, .. } = &mut dropped[0] {
        calls[0].id = Some("dropped-call".into());
    }
    if let SessionLogEntry::ToolResults { results, .. } = &mut dropped[1] {
        results[0].id = Some("dropped-call".into());
        results[0].output = output;
    }
    let mut entries = vec![
        entry(MessageRole::User, "archived question"),
        entry(MessageRole::Assistant, "archived answer"),
        suffix[0].clone(),
    ];
    entries.extend(dropped);
    entries.extend_from_slice(&suffix[1..]);
    setup(&entries, "system instructions")
}

fn split_compacted_session() -> (GlobalConfig, Arc<CaptureLog>) {
    let (config, log) = session_with_large_middle();
    let (id, plan, expected) = {
        let guard = config.read();
        let sess = guard.session.as_ref().unwrap();
        let budget = sess.model().messages_tokens(&sess.messages[4..])
            + sess.model().messages_tokens(&sess.messages[2..=2]);
        let plan = crate::config::compaction::plan_compaction(
            &sess.messages,
            sess.model(),
            &crate::config::compaction::CompactionParams {
                keep_recent_turns: 3,
                keep_recent_tokens: budget,
                tool_output_max_chars: 2000,
            },
            180_000,
        );
        assert_eq!(plan.prompt_idx, Some(2));
        assert_eq!(plan.tail_from, 4);
        let expected = vec![
            sess.messages[2].clone(),
            sess.messages[4].clone(),
            sess.messages[5].clone(),
        ];
        (sess.id.clone(), plan, expected)
    };
    assert!(Config::apply_compaction_summary(
        &config,
        &id,
        "completed prior work".into(),
        plan
    ));
    assert_eq!(
        view(&config.read().session.as_ref().unwrap().messages),
        view(&expected)
    );
    (config, log)
}

#[test]
fn compaction_noncontiguous_kept_set_round_trips_through_log() {
    let (config, log) = split_compacted_session();
    let messages = {
        let guard = config.read();
        let sess = guard.session.as_ref().unwrap();
        assert_eq!(sess.compressed_messages.len(), 3);
        serde_json::to_value(&sess.messages).unwrap()
    };
    let entries = log.0.lock().unwrap();
    let marker = entries
        .iter()
        .rposition(|(_, e)| matches!(e, SessionLogEntry::Compress { .. }))
        .unwrap();
    assert_eq!(entries.len() - marker, 5); // Compress, prompt, calls, results, reply
    assert!(!format!("{:?}", &entries[marker..]).contains("dropped-call"));
    drop(entries);
    reload(&config, &log);
    let guard = config.read();
    let sess = guard.session.as_ref().unwrap();
    assert_eq!(serde_json::to_value(&sess.messages).unwrap(), messages);
}

fn split_compacted_request() -> ChatCompletionsData {
    let (config, _log) = split_compacted_session();
    let client = MockClient::builder().build();
    let mut input = from_str(&config, "next request", None);
    prepare_completion_data(&mut input, &config, client.model(), false, &client).unwrap()
}

#[derive(Debug, Clone, Copy)]
enum Provider {
    Claude,
    OpenAI,
    Gemini,
}

impl Provider {
    fn body(self, data: ChatCompletionsData) -> Value {
        match self {
            Self::Claude => harnx_client::claude::claude_build_chat_completions_body(
                data,
                &Model::new("claude", "claude-sonnet-4"),
                &Default::default(),
            )
            .unwrap(),
            Self::OpenAI => harnx_client::openai::openai_build_chat_completions_body(
                data,
                &Model::new("openai", "gpt-4o"),
            ),
            Self::Gemini => harnx_client::vertexai::gemini_build_chat_completions_body(
                data,
                &Model::new("gemini", "gemini-2.5-pro"),
                Default::default(),
            )
            .unwrap(),
        }
    }

    fn messages(self, body: &Value) -> &[Value] {
        let key = match self {
            Self::Gemini => "contents",
            _ => "messages",
        };
        body[key].as_array().unwrap()
    }

    fn assistant_role(self) -> &'static str {
        match self {
            Self::Gemini => "model",
            _ => "assistant",
        }
    }
}

fn assert_roles(messages: &[Value], expected: &[&str]) {
    let roles: Vec<_> = messages
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, expected);
}

fn assert_text_contains(value: &Value, text: &str) {
    assert!(
        value.to_string().contains(text),
        "expected {text:?} in {value}"
    );
}

fn assert_claude_tool_round(messages: &[Value]) {
    let call = messages[0]["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["type"] == "tool_use")
        .unwrap();
    let result = &messages[1]["content"][0];
    assert_eq!(call["id"], "edit-1");
    assert_eq!(result["type"], "tool_result");
    assert_eq!(result["tool_use_id"], call["id"]);
}

fn assert_openai_tool_round(messages: &[Value]) {
    let call_id = &messages[0]["tool_calls"][0]["id"];
    assert_eq!(call_id, "edit-1");
    assert_eq!(messages[1]["tool_call_id"], *call_id);
}

fn assert_gemini_tool_round(messages: &[Value]) {
    let call = messages[0]["parts"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|p| p.get("functionCall"))
        .unwrap();
    let result = &messages[1]["parts"][0]["functionResponse"];
    assert_eq!(call["id"], "edit-1");
    assert_eq!(result["name"], call["name"]);
    assert_eq!(result["response"]["content"], json!({"ok": true}));
}

#[test]
fn compaction_claude_serialized_request_preserves_prompt_and_tool_round() {
    let body = Provider::Claude.body(split_compacted_request());
    let messages = Provider::Claude.messages(&body);
    assert_roles(
        messages,
        &["user", "user", "assistant", "user", "assistant", "user"],
    );
    assert_text_contains(&messages[0]["content"], "completed prior work");
    assert_text_contains(&messages[1]["content"], "remove hex");
    assert_claude_tool_round(&messages[2..4]);
    assert_text_contains(&messages[4]["content"], "hex removed and verified");
    assert!(!body.to_string().contains("dropped-call"));
}

#[test]
fn compaction_openai_serialized_request_preserves_prompt_and_tool_round() {
    let body = Provider::OpenAI.body(split_compacted_request());
    let messages = Provider::OpenAI.messages(&body);
    assert_roles(
        messages,
        &[
            "system",
            "user",
            "user",
            "assistant",
            "tool",
            "assistant",
            "user",
        ],
    );
    assert_text_contains(&messages[1]["content"], "completed prior work");
    assert_eq!(messages[2]["content"], "remove hex");
    assert_openai_tool_round(&messages[3..5]);
    assert_eq!(messages[5]["content"], "hex removed and verified");
    assert!(!body.to_string().contains("dropped-call"));
}

#[test]
fn compaction_gemini_serialized_request_preserves_prompt_and_tool_round() {
    let body = Provider::Gemini.body(split_compacted_request());
    let messages = Provider::Gemini.messages(&body);
    assert_roles(
        messages,
        &["user", "user", "model", "user", "model", "user"],
    );
    assert_text_contains(&messages[0]["parts"], "completed prior work");
    assert_eq!(messages[1]["parts"][0]["text"], "remove hex");
    assert_gemini_tool_round(&messages[2..4]);
    assert_eq!(messages[4]["parts"][0]["text"], "hex removed and verified");
    assert!(!body.to_string().contains("dropped-call"));
}

#[tokio::test]
async fn compaction_summarizer_receives_only_complement_of_prompt_and_tail() {
    let (config, log) = session_with_large_middle();
    let client = Arc::new(
        MockClient::builder()
            .add_turn(
                MockTurnBuilder::new()
                    .add_text_chunk("middle summarized")
                    .build(),
            )
            .build(),
    );
    let _guard = TestStateGuard::new(Some(client.clone())).await;
    Config::compact_session(&config).await.unwrap();
    let history = client.conversation_history();
    let transcript = history.conversation_history[0]
        .messages
        .iter()
        .find(|m| m.role == MessageRole::User)
        .unwrap()
        .content
        .to_text();
    assert!(transcript.contains("archived question"));
    assert!(transcript.contains("word word"));
    assert!(!transcript.contains("remove hex"));
    assert!(!transcript.contains("hex removed and verified"));
    assert!(!transcript.contains("\"ok\":true"));
    let kept = view(&config.read().session.as_ref().unwrap().messages);
    assert_eq!(kept.len(), 3);
    reload(&config, &log);
    assert_eq!(
        view(&config.read().session.as_ref().unwrap().messages),
        kept
    );
}

fn reply_only_compacted_session() -> (GlobalConfig, Arc<CaptureLog>) {
    let (config, log) = setup(&tool_suffix(), "");
    let (id, plan) = {
        let guard = config.read();
        let sess = guard.session.as_ref().unwrap();
        let budget = sess.model().messages_tokens(&sess.messages[..1])
            + sess.model().messages_tokens(&sess.messages[2..]);
        let plan = crate::config::compaction::plan_compaction(
            &sess.messages,
            sess.model(),
            &crate::config::compaction::CompactionParams {
                keep_recent_turns: 3,
                keep_recent_tokens: budget,
                tool_output_max_chars: 2000,
            },
            180_000,
        );
        assert_eq!(plan.kept_indices(sess.messages.len()), vec![0, 2]);
        (sess.id.clone(), plan)
    };
    assert!(Config::apply_compaction_summary(
        &config,
        &id,
        "middle summarized".into(),
        plan
    ));
    (config, log)
}

fn assert_reply_only_request(config: &GlobalConfig, provider: Provider) {
    let client = MockClient::builder().build();
    let mut input = from_str(config, "", None);
    input.skip_user_log_append = true;
    let data = prepare_completion_data(&mut input, config, client.model(), false, &client).unwrap();
    let body = provider.body(data);
    let messages = provider.messages(&body);
    assert_roles(messages, &["user", "user", provider.assistant_role()]);
    for (message, text) in messages.iter().zip([
        "Earlier conversation summary:",
        "remove hex",
        "hex removed and verified",
    ]) {
        assert_text_contains(message, text);
    }
    assert_text_contains(&messages[0], "middle summarized");
}

#[test]
fn compaction_reply_only_tail_serializes_for_claude_openai_and_gemini() {
    for provider in [Provider::Claude, Provider::OpenAI, Provider::Gemini] {
        let (config, log) = reply_only_compacted_session();
        assert_reply_only_request(&config, provider);
        reload(&config, &log);
        assert_reply_only_request(&config, provider);
    }
}

/// Issue #2493: a mid-turn injected prompt pushed into session memory keeps
/// its durable identity (id + log seq), so a later compaction re-logs it as a
/// copy of the admitted prompt — not as an ID-less user row that admission
/// checks would treat as a fresh unadmitted prompt.
#[test]
fn injected_durable_messages_survive_compaction_with_identity() {
    let entries = vec![
        entry(MessageRole::User, "seed"),
        entry(MessageRole::Assistant, "answer"),
    ];
    let (config, log) = setup(&entries, "");
    let mut session = config
        .write()
        .session
        .take()
        .expect("setup attaches a session");

    // Mirror what `build_mid_turn_injection_callback` selects: the durable
    // "late message" row, id and log seq intact.
    let late = Message::new(
        MessageRole::User,
        MessageContent::Text("late message".into()),
    )
    .with_id("late-1")
    .with_log_seq(3);
    let mut input = from_str(&config, "seed", None);
    input.skip_user_log_append = true;
    input.set_injected_user_messages(vec![late]);

    session::prepare_input(&mut session, &mut input).unwrap();
    let pushed = session
        .messages
        .last()
        .expect("injection pushed into memory");
    assert_eq!(pushed.id.as_deref(), Some("late-1"));
    assert_eq!(pushed.log_seq, Some(3));

    let before = log.0.lock().unwrap().len();
    session::compress_keeping_recent(&mut session, "summary".into(), 1);
    let relogged = log.0.lock().unwrap()[before..].to_vec();
    let late_rows: Vec<&SessionLogEntry> = relogged
        .iter()
        .filter(|(_, entry)| {
            matches!(
                entry,
                SessionLogEntry::Message { role, content, .. }
                    if *role == MessageRole::User && content.to_text() == "late message"
            )
        })
        .map(|(_, entry)| entry)
        .collect();
    assert!(
        !late_rows.is_empty(),
        "compaction must re-log the kept injected prompt"
    );
    for entry in late_rows {
        let SessionLogEntry::Message { id, .. } = entry else {
            unreachable!("filtered to Message rows");
        };
        assert_eq!(
            id.as_deref(),
            Some("late-1"),
            "compaction must preserve the injected prompt's admission identity"
        );
    }
}

/// Issue #2493: replay-synthesized runtime notes (sub-agent starts, cancels)
/// carry a structured runtime-note id, so a compaction re-log persists them as
/// non-executable context rows instead of ID-less user rows.
#[test]
fn replayed_runtime_notes_carry_non_executable_identity_through_compaction() {
    assert_replayed_note_survives_compaction(SessionLogEntry::SubAgentStarted {
        agent: "child".into(),
        session_id: "child-session".into(),
        invocation_id: None,
        tool_call_id: None,
        started_at: None,
    });
}

/// Issue #2493: a replayed `Cancel` synthesizes the interrupt runtime note,
/// which must carry the structured runtime-note id through a compaction
/// re-log — otherwise the copy is an ID-less user row that admission checks
/// treat as an unadmitted prompt.
#[test]
fn replayed_cancel_note_carries_non_executable_identity_through_compaction() {
    assert_replayed_note_survives_compaction(SessionLogEntry::Cancel {
        fence_token: 0,
        cancellation_id: None,
        requested_by: None,
        timestamp: None,
    });
}

fn assert_replayed_note_survives_compaction(note_event: SessionLogEntry) {
    let entries = vec![entry(MessageRole::User, "seed"), note_event];
    let (config, log) = setup(&entries, "");
    let mut session = config
        .write()
        .session
        .take()
        .expect("setup attaches a session");

    let note = session
        .messages
        .last()
        .expect("replay synthesized the note");
    assert!(
        note.id
            .as_deref()
            .is_some_and(harnx_core::runtime_note::is_runtime_note_message_id),
        "replayed runtime notes must carry a runtime-note id, got {:?}",
        note.id
    );

    let before = log.0.lock().unwrap().len();
    session::compress_keeping_recent(&mut session, "summary".into(), 1);
    let relogged = log.0.lock().unwrap()[before..].to_vec();
    let note_rows: Vec<&SessionLogEntry> = relogged
        .iter()
        .filter(|(_, entry)| {
            matches!(
                entry,
                SessionLogEntry::Message { id, role, content, .. }
                    if role.is_user()
                        && harnx_core::runtime_note::is_runtime_note_id(id.as_deref())
                        && content.to_text().starts_with(harnx_core::runtime_note::RUNTIME_NOTE_PREFIX)
            )
        })
        .map(|(_, entry)| entry)
        .collect();
    assert_eq!(
        note_rows.len(),
        1,
        "compaction must re-log the runtime note exactly once, got {relogged:?}"
    );
}

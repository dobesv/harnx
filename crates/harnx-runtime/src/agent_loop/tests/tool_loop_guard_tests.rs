//! A model that keeps requesting the same call gets notes, a refusal, and
//! then a stopped turn.

use super::*;
use crate::client::MessageContent;
use harnx_core::event::{ModelEvent, ToolEvent};
use harnx_core::loop_guard::{RepetitionStop, RepetitionTerminal};

type Rounds = Arc<Mutex<Vec<Vec<ToolResult>>>>;

/// Arguments for the repeated call, with the keys in a different order each
/// round, as Gemini produced them.
fn read_arguments(n: usize) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    let pairs = [
        ("path", json!("a.rs")),
        ("offset", json!(70)),
        ("limit", json!(70)),
    ];
    for i in 0..3 {
        let (key, value) = pairs[(i + n) % 3].clone();
        map.insert(key.to_string(), value);
    }
    serde_json::Value::Object(map)
}

/// A model that requests one `fs_read` with `arguments(n)` on each of its
/// first `limit` calls and then answers with text. `calls` counts its calls.
fn repeating_model(
    calls: &Arc<AtomicUsize>,
    limit: usize,
    arguments: fn(usize) -> serde_json::Value,
) -> AgentCallFn {
    let counter = calls.clone();
    Arc::new(move |_input, _config, _abort| {
        let counter = counter.clone();
        Box::pin(async move {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let calls = if n < limit {
                vec![ToolCall::new(
                    "fs_read".to_string(),
                    arguments(n),
                    Some(format!("call_{n}")),
                    None,
                )]
            } else {
                vec![]
            };
            Ok((
                "done".to_string(),
                None,
                calls,
                CompletionTokenUsage::default(),
            ))
        })
    })
}

/// Records the results of every round that reaches `on_tool_round`.
fn recorded_rounds() -> (Rounds, OnToolRoundFn) {
    recorded_rounds_with_message(None)
}

/// Like [`recorded_rounds`], and once `after_round` rounds have run, delivers
/// a user message mid-loop the way the TUI delivers one queued during a turn.
fn recorded_rounds_with_message(after_round: Option<usize>) -> (Rounds, OnToolRoundFn) {
    let rounds: Rounds = Arc::default();
    let seen = rounds.clone();
    let on_tool_round: OnToolRoundFn = Arc::new(move |merged, results| {
        let mut seen = seen.lock().unwrap();
        seen.push(results.to_vec());
        if Some(seen.len()) == after_round {
            merged.set_injected_user_text("keep going".to_string());
        }
        Box::pin(async { Ok(()) })
    });
    (rounds, on_tool_round)
}

#[tokio::test(flavor = "multi_thread")]
async fn repeated_identical_calls_are_noted_refused_and_then_stopped() {
    let _sink_guard = SINK_LOCK.lock().await;
    let _guard = crate::client::TestStateGuard::new(None).await;
    let tmp = TempDir::new().unwrap();
    let config = replay_test_config(&tmp);
    let model_calls = Arc::new(AtomicUsize::new(0));
    // A backstop so a broken guard fails the assertions instead of hanging.
    let call_fn = repeating_model(&model_calls, 12, read_arguments);
    let (rounds, on_tool_round) = recorded_rounds();

    let ctx = make_test_context(config.clone(), call_fn, on_tool_round);
    let input = crate::config::input::from_str(&config, "read it", None);
    let sink = Arc::new(CollectingSink::default());
    let result =
        harnx_core::sink::with_agent_event_sink(sink.clone(), run_agent_loop(&ctx, input)).await;
    let Err(error) = result else {
        panic!("the guard must stop the turn");
    };

    assert!(
        error
            .chain()
            .any(|cause| cause.downcast_ref::<RepetitionStop>().is_some()),
        "unexpected error: {error:#}"
    );
    assert_eq!(
        model_calls.load(Ordering::SeqCst),
        6,
        "4 runs, 1 refusal, then the stop"
    );

    let rounds = rounds.lock().unwrap();
    assert_eq!(
        rounds.len(),
        5,
        "the stopped round never reaches on_tool_round"
    );
    let output = |round: usize| rounds[round][0].output.clone();
    assert!(output(0).get("harnx_note").is_none());
    for round in 1..4 {
        let note = output(round)["harnx_note"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(
            note.starts_with("[harnx] Same call and same result"),
            "round {round}: {note}"
        );
    }
    assert_eq!(output(4)["loop_guard"], json!("refused"));

    assert_stopped_call_has_a_result(&config);
    assert_live_events(&sink);
}

/// The stopped call never runs but still gets a persisted result, so the
/// transcript keeps one result per call.
fn assert_stopped_call_has_a_result(config: &GlobalConfig) {
    let config = config.read();
    let session = config.session.as_ref().expect("session attached above");
    let Some(MessageContent::ToolCalls(stopped)) = session.messages.last().map(|m| &m.content)
    else {
        panic!("the stopped round must be the last message");
    };
    let stopped: Vec<_> = stopped
        .tool_results
        .iter()
        .map(|r| (r.call.id.clone().unwrap(), r.output["loop_guard"].clone()))
        .collect();
    assert_eq!(stopped, [("call_5".to_string(), json!("stopped"))]);
}

/// The refusal shows as a blocked call and each note as a notice. The stop
/// shows only its sentence: the marker is for parents, not people.
fn assert_live_events(sink: &CollectingSink) {
    let events = sink.events.lock().unwrap();
    let count = |wanted: fn(&AgentEvent) -> bool| events.iter().filter(|(e, _)| wanted(e)).count();
    assert_eq!(
        count(|e| matches!(e, AgentEvent::Tool(ToolEvent::Blocked { id, .. }) if id == "call_4")),
        1,
        "the refused call is shown as blocked"
    );
    assert_eq!(
        count(
            |e| matches!(e, AgentEvent::Notice(NoticeEvent::Warning(text))
            if text.starts_with("fs_read: [harnx] Same call and same result"))
        ),
        3,
        "each note is shown live"
    );
    let live_errors: Vec<_> = events
        .iter()
        .filter_map(|(event, _)| match event {
            AgentEvent::Model(ModelEvent::Error(text)) => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        live_errors,
        [RepetitionTerminal::tool_calls("fs_read", 4).sentence()]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_arriving_mid_loop_starts_the_count_over() {
    let _sink_guard = SINK_LOCK.lock().await;
    let _guard = crate::client::TestStateGuard::new(None).await;
    let tmp = TempDir::new().unwrap();
    let config = replay_test_config(&tmp);
    let call_fn = repeating_model(&Arc::default(), 5, read_arguments);
    let (rounds, on_tool_round) = recorded_rounds_with_message(Some(4));

    let ctx = make_test_context(config.clone(), call_fn, on_tool_round);
    let input = crate::config::input::from_str(&config, "read it", None);
    run_agent_loop(&ctx, input)
        .await
        .expect("five identical calls with a message between them do not stop the turn");

    let rounds = rounds.lock().unwrap();
    assert_eq!(rounds.len(), 5);
    let output = |round: usize| rounds[round][0].output.clone();
    assert!(
        output(3).get("harnx_note").is_some(),
        "the fourth call is noted before the message arrives"
    );
    let fifth = output(4);
    assert!(
        fifth.get("harnx_note").is_none() && fifth.get("loop_guard").is_none(),
        "the fifth call starts a new count: {fifth}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn disabled_guard_leaves_repeated_calls_alone() {
    let _sink_guard = SINK_LOCK.lock().await;
    let _guard = crate::client::TestStateGuard::new(None).await;
    let tmp = TempDir::new().unwrap();
    let config = replay_test_config(&tmp);
    config.write().loop_detection.tool_calls = false;
    let call_fn = repeating_model(&Arc::default(), 8, |_| json!({"path": "a.rs"}));
    let (rounds, on_tool_round) = recorded_rounds();

    let ctx = make_test_context(config.clone(), call_fn, on_tool_round);
    let input = crate::config::input::from_str(&config, "read it", None);
    run_agent_loop(&ctx, input)
        .await
        .expect("no guard, no stop");

    let rounds = rounds.lock().unwrap();
    assert_eq!(rounds.len(), 8);
    assert!(rounds.iter().all(
        |r| r[0].output.get("harnx_note").is_none() && r[0].output.get("loop_guard").is_none()
    ));
}

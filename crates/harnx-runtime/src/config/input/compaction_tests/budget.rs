use super::*;

const DATA_URL: &str = "data:image/png;base64,AQIDBA==";
const OMITTED_OUTPUT: &str = "[edit: completed; output omitted]";

fn budgeted_session(stream: bool) -> (GlobalConfig, Arc<CaptureLog>, Vec<Message>) {
    let output = format!(
        "dropped head {DATA_URL} {}dropped tail",
        "word ".repeat(10000)
    );
    let (config, log) = session_with_middle_output(json!({
        "_meta": {"harnx.dev/compaction": output}
    }));
    let expected = {
        let mut guard = config.write();
        guard.stream = stream;
        let session = guard.session.as_mut().unwrap();
        session.compaction_summary = Some(format!("prior work {DATA_URL}"));
        [2, 4, 5]
            .into_iter()
            .map(|index| session.messages[index].clone())
            .collect()
    };
    (config, log, expected)
}

fn summary_client(limit: usize) -> Arc<MockClient> {
    let mut model = Model::new("mock", "summarizer");
    model.data_mut().max_input_tokens = Some(limit);
    Arc::new(
        MockClient::builder()
            .model(model)
            .add_turn(
                MockTurnBuilder::new()
                    .add_text_chunk("middle summarized")
                    .build(),
            )
            .build(),
    )
}

fn assert_dropped_complement(transcript: &str) {
    for kept in ["remove hex", "hex removed and verified", "\"ok\":true"] {
        assert!(
            !transcript.contains(kept),
            "kept content leaked: {transcript}"
        );
    }
    for dropped in [
        "archived question",
        "archived answer",
        "── tool call: edit ──",
    ] {
        assert!(
            transcript.contains(dropped),
            "dropped content missing: {transcript}"
        );
    }
}

fn assert_sanitized_request(client: &MockClient) -> String {
    let history = client.conversation_history();
    let request = &history.conversation_history[0];
    let limit = client.model().max_input_tokens().unwrap() * 90 / 100;
    assert!(client.model().messages_tokens(&request.messages) + 1024 <= limit);
    let transcript = request.messages[1].content.to_text();
    assert!(transcript.contains("prior work [image/png data: 4 bytes]"));
    assert!(!transcript.contains("AQIDBA=="));
    assert_dropped_complement(&transcript);
    transcript
}

fn assert_retention_and_reload(config: &GlobalConfig, log: &CaptureLog, expected: &[Message]) {
    let guard = config.read();
    let session = guard.session.as_ref().unwrap();
    assert_eq!(view(&session.messages), view(expected));
    let archived = serde_json::to_string(&session.compressed_messages).unwrap();
    assert!(
        archived.contains(DATA_URL),
        "sanitization must not edit source messages"
    );
    drop(guard);
    reload(config, log);
    assert_eq!(
        view(&config.read().session.as_ref().unwrap().messages),
        view(expected)
    );
}

#[tokio::test]
async fn compaction_budgets_sanitized_complement_without_changing_kept_set() {
    for stream in [false, true] {
        let (config, log, expected) = budgeted_session(stream);
        let client = summary_client(1600);
        let _guard = TestStateGuard::new(Some(client.clone())).await;
        Config::compact_session(&config).await.unwrap();
        let transcript = assert_sanitized_request(&client);
        assert!(transcript.contains(OMITTED_OUTPUT), "{transcript}");
        assert_retention_and_reload(&config, &log, &expected);
    }
}

#[tokio::test]
async fn compaction_sanitizes_dropped_output_before_budget_reduction_is_needed() {
    let (config, log, expected) = budgeted_session(false);
    let client = summary_client(100_000);
    let _guard = TestStateGuard::new(Some(client.clone())).await;
    Config::compact_session(&config).await.unwrap();
    let transcript = assert_sanitized_request(&client);
    assert!(transcript.contains("dropped head [image/png data: 4 bytes]"));
    assert!(!transcript.contains(OMITTED_OUTPUT));
    assert_retention_and_reload(&config, &log, &expected);
}

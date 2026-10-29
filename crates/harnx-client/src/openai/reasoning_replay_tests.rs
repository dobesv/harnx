use super::*;
use harnx_core::tool::ToolResult;

#[test]
fn whitespace_only_reasoning_remains_absent_on_generic_endpoints() {
    let output = openai_extract_chat_completions(
        &response("call_1", "Read.", " \n "),
        &Model::new("proxy", "generic"),
    )
    .unwrap();
    assert_eq!(output.text, "Read.");
    assert!(output.thought.is_none());
}

#[test]
fn streamed_reasoning_is_replayed_separately_on_tool_continuation() {
    for name in ["MiniMax-M3.1-Flash", "glm-5.3", "us.zai.glm-5.3"] {
        let model = Model::new("proxy", name);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut handler = SseHandler::new(tx, harnx_core::abort::create_abort_signal());
        let mut state = OpenAiStreamState::default();
        for data in [
            json!({"choices": [{"delta": {"reasoning_content": "  First "}}]}),
            json!({"choices": [{"delta": {"reasoning_content": "reasoning.\n", "content": "Read."}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "call_1", "function": {"name": "read", "arguments": "{}"}}]}}]}),
        ] {
            openai_handle_stream_event(&mut state, &mut handler, &data, &model).unwrap();
        }
        openai_emit_pending_tool_call(&mut state, &mut handler).unwrap();
        let (text, thought, calls, _) = handler.take();
        let results = calls
            .into_iter()
            .map(|call| ToolResult::new(call, json!({"ok": true})))
            .collect();
        let body = continue_calls(MessageContentToolCalls::new(results, text, thought), &model);
        assert_eq!(
            (
                &body["messages"][0]["content"],
                &body["messages"][0]["reasoning_content"]
            ),
            (&json!("Read."), &json!("  First reasoning.\n"))
        );
        assert_eq!(body["messages"][1]["tool_call_id"], "call_1");
    }
}

#[test]
fn merged_tool_images_remain_next_to_the_round_that_returned_them() {
    let result = |id: &str| {
        let mut result = ToolResult::new(
            ToolCall::new("read".into(), json!({}), Some(id.into()), None),
            json!({}),
        );
        result.content = vec![harnx_core::message::MessageContentPart::ImageUrl {
            image_url: harnx_core::message::ImageUrl {
                url: format!("https://example.invalid/{id}.png"),
            },
        }];
        result
    };
    let mut calls = MessageContentToolCalls::new(
        vec![result("first")],
        "first".into(),
        Some("first thought".into()),
    );
    calls.merge(
        vec![result("second")],
        "second".into(),
        Some("second thought".into()),
    );
    let body = continue_calls(calls, &Model::new("minimax", "MiniMax-M3.1-Flash"));
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 6);
    for (offset, id) in [(0, "first"), (3, "second")] {
        assert_eq!(messages[offset + 1]["tool_call_id"], id);
        assert_eq!(
            messages[offset + 2]["content"][0]["image_url"]["url"],
            format!("https://example.invalid/{id}.png")
        );
    }
}

#[test]
fn malformed_deserialized_round_counts_use_legacy_replay_without_reasoning() {
    for count in [0, 3, usize::MAX] {
        let result = |id: &str| {
            ToolResult::new(
                ToolCall::new("read".into(), json!({}), Some(id.into()), None),
                json!({}),
            )
        };
        let mut calls = MessageContentToolCalls::new(
            vec![result("first")],
            "first".into(),
            Some("first thought".into()),
        );
        calls.merge(
            vec![result("second")],
            "second".into(),
            Some("second thought".into()),
        );
        let mut serialized = serde_json::to_value(calls).unwrap();
        serialized["rounds"][0]["tool_count"] = json!(count);
        let calls = serde_json::from_value(serialized).unwrap();
        let body = continue_calls(calls, &Model::new("minimax", "MiniMax-M3.1-Flash"));
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 4);
        for assistant in messages
            .iter()
            .filter(|message| message["role"] == "assistant")
        {
            assert_eq!(assistant["tool_calls"].as_array().unwrap().len(), 1);
            assert!(assistant.get("reasoning_content").is_none());
        }
    }
}

fn response(id: &str, text: &str, thought: &str) -> Value {
    json!({"choices": [{"message": {
        "content": text, "reasoning_content": thought,
        "tool_calls": [{"id": id, "type": "function", "function": {"name": "read", "arguments": "{}"}}]
    }}]})
}

fn continue_calls(calls: MessageContentToolCalls, model: &Model) -> Value {
    openai_build_chat_completions_body(
        ChatCompletionsData {
            messages: vec![Message::new(
                MessageRole::Assistant,
                MessageContent::ToolCalls(calls),
            )],
            temperature: None,
            top_p: None,
            functions: None,
            stream: false,
            attachments_dir: None,
        },
        model,
    )
}

#[test]
fn extracted_reasoning_stays_with_its_own_round_on_tool_continuation() {
    for name in ["MiniMax-M3.1-Flash", "glm-5.3", "us.zai.glm-5.3"] {
        let model = Model::new("proxy", name);
        let first = openai_extract_chat_completions(
            &response("call_1", "Read first.", "  First reasoning.\n"),
            &model,
        )
        .unwrap();
        let second = openai_extract_chat_completions(
            &response("call_2", "Read second.", "Second reasoning."),
            &model,
        )
        .unwrap();
        let results = |output: &ChatCompletionsOutput| {
            output
                .tool_calls
                .iter()
                .cloned()
                .map(|call| ToolResult::new(call, json!({"ok": true})))
                .collect()
        };
        let mut calls = MessageContentToolCalls::new(
            results(&first),
            first.text.clone(),
            first.thought.clone(),
        );
        let body = continue_calls(calls.clone(), &model);
        assert_eq!(
            body["messages"][0]["reasoning_content"],
            "  First reasoning.\n"
        );
        calls.merge(results(&second), second.text, second.thought);
        // Boundaries must also survive a stored-message round trip.
        let calls = serde_json::from_value(serde_json::to_value(calls).unwrap()).unwrap();
        let body = continue_calls(calls, &model);
        assert_eq!(
            body["messages"],
            json!([
                {"role": "assistant", "content": "Read first.", "reasoning_content": "  First reasoning.\n", "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "read", "arguments": "{}"}}]},
                {"role": "tool", "content": "{\"ok\":true}", "tool_call_id": "call_1"},
                {"role": "assistant", "content": "Read second.", "reasoning_content": "Second reasoning.", "tool_calls": [{"id": "call_2", "type": "function", "function": {"name": "read", "arguments": "{}"}}]},
                {"role": "tool", "content": "{\"ok\":true}", "tool_call_id": "call_2"}
            ]),
            "{name}"
        );
    }
}

#[test]
fn generic_endpoints_and_legacy_merged_rounds_do_not_invent_reasoning_associations() {
    for (name, sequence) in [("gpt-4o", false), ("MiniMax-M3.1-Flash", true)] {
        let call = ToolCall::new("read".into(), json!({}), Some("call_1".into()), None);
        let mut calls = MessageContentToolCalls::new(
            vec![ToolResult::new(call, json!({}))],
            "old aggregate".into(),
            Some("old aggregate reasoning".into()),
        );
        calls.sequence = sequence;
        let body = continue_calls(calls, &Model::new("proxy", name));
        assert!(body["messages"][0].get("reasoning_content").is_none());
        assert_eq!(body["messages"][1]["tool_call_id"], "call_1");
    }
}

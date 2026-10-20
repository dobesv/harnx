use super::*;
use crate::client::TestStateGuard;
use crate::config::{Config, ConfigLock};
use crate::test_utils::{MockClient, MockTurnBuilder};
use harnx_core::message::{Message, MessageContent, MessageRole};
use std::sync::Arc;

fn model(limit: Option<usize>) -> Model {
    let mut model = Model::new("mock", "summarizer");
    model.data_mut().max_input_tokens = limit;
    model
}

fn source() -> Transcript {
    Transcript::new(
        &[
            Message::new(
                MessageRole::User,
                MessageContent::Text(format!("old {} tail", "request ".repeat(2000))),
            ),
            Message::new(
                MessageRole::User,
                MessageContent::Text("latest question".into()),
            ),
            Message::new(
                MessageRole::Assistant,
                MessageContent::Text("latest answer".into()),
            ),
        ],
        2000,
        Some("prior summary data:image/png;base64,AQID"),
    )
}

fn input(prompt: &str) -> Input {
    let mut input = Input::new(
        String::new(),
        (String::new(), vec![]),
        AgentConfig::from_prompt(prompt),
    );
    input.with_agent = true;
    input
}

#[test]
fn budget_is_ninety_percent_of_known_limit_else_150k_without_overflow() {
    assert_eq!(input_budget(&model(Some(128_000))), 115_200);
    assert_eq!(input_budget(&model(Some(1_000_000))), 900_000);
    assert_eq!(input_budget(&model(None)), 150_000);
    assert_eq!(input_budget(&model(Some(0))), 0);
    assert_eq!(
        input_budget(&model(Some(usize::MAX))),
        (usize::MAX as u128 * 90 / 100) as usize
    );
}

#[test]
fn request_budget_includes_system_prompt_model_prefix_and_retry_note() {
    let config = Arc::new(ConfigLock::new(Config::default()));
    let mut input = input(&"summarize ".repeat(70));
    input.transient_note = Some("avoid repeating the summary".into());
    let source = source();
    for limit in (1500..=4000).step_by(199) {
        let mut model = model(Some(limit));
        model.data_mut().system_prompt_prefix = Some(vec!["provider system prefix ".repeat(15)]);
        input.agent.set_model(model.clone());
        let data = prepare_request(&mut input, &config, &model, &source).unwrap();
        let tokens = model.messages_tokens(&data.messages) + REQUEST_OVERHEAD;
        assert!(tokens <= input_budget(&model), "{limit}: {tokens}");
        assert!(data.messages[0]
            .content
            .to_text()
            .contains("provider system prefix"));
        let text = data.messages[1].content.to_text();
        assert!(text.contains("latest question") && text.contains("latest answer"));
        assert!(!text.contains("AQID"));
        assert_eq!(
            data.messages.last().unwrap().content.to_text(),
            "avoid repeating the summary"
        );
    }
}

#[test]
fn rebudgeting_uses_original_source_for_smaller_and_larger_fallback_models() {
    let config = Arc::new(ConfigLock::new(Config::default()));
    let mut input = input("summarize");
    let transcript = source();
    let small = prepare_request(&mut input, &config, &model(Some(1500)), &transcript).unwrap();
    assert!(small.messages[1]
        .content
        .to_text()
        .contains("chars omitted"));
    let large = prepare_request(&mut input, &config, &model(Some(100_000)), &transcript).unwrap();
    assert!(!large.messages[1]
        .content
        .to_text()
        .contains("chars omitted"));
    let small_again =
        prepare_request(&mut input, &config, &model(Some(1500)), &transcript).unwrap();
    assert_eq!(
        serde_json::to_value(&small.messages).unwrap(),
        serde_json::to_value(&small_again.messages).unwrap()
    );
}

#[test]
fn oversized_system_prompt_is_unchanged_and_transcript_minimized_without_size_error() {
    let config = Arc::new(ConfigLock::new(Config::default()));
    let prompt = "system instructions ".repeat(2000);
    let mut input = input(&prompt);
    let model = model(Some(128));
    let data = prepare_request(&mut input, &config, &model, &source()).unwrap();
    assert_eq!(data.messages[0].content.to_text(), prompt);
    let text = data.messages[1].content.to_text();
    assert!(text.contains("[1 earlier user prompts omitted]"));
    assert!(text.contains("── user ──"));
    assert!(estimate_token_length(&text) < 60);
    assert!(
        model.guard_max_input_tokens(&data.messages).is_err(),
        "ordinary guard must still reject oversized requests"
    );
}

#[tokio::test]
async fn selected_summarizer_model_budget_is_used_in_streaming_and_nonstreaming_calls() {
    for stream in [false, true] {
        let mock_model = model(Some(1600));
        let mock = Arc::new(
            MockClient::builder()
                .model(mock_model.clone())
                .add_turn(MockTurnBuilder::new().add_text_chunk("Summary.").build())
                .build(),
        );
        let _guard = TestStateGuard::new(Some(mock.clone())).await;
        let config = Config {
            data: harnx_core::config_data::ConfigData {
                stream,
                ..Default::default()
            },
            ..Default::default()
        };
        let config = Arc::new(ConfigLock::new(config));
        let mut agent = AgentConfig::from_prompt("summarize");
        agent.set_model(model(Some(1_000_000)));
        assert_eq!(
            fetch_summary(source(), agent, &config).await.unwrap(),
            "Summary."
        );
        let history = mock.conversation_history();
        let data = &history.conversation_history[0];
        assert!(
            mock_model.messages_tokens(&data.messages) + REQUEST_OVERHEAD
                <= input_budget(&mock_model)
        );
        assert!(data.messages[1].content.to_text().contains("chars omitted"));
    }
}

#[tokio::test]
async fn oversized_system_prompt_is_sent_instead_of_rejected_by_local_guard() {
    let mock = Arc::new(
        MockClient::builder()
            .model(model(Some(100)))
            .add_turn(MockTurnBuilder::new().add_text_chunk("Summary.").build())
            .build(),
    );
    let _guard = TestStateGuard::new(Some(mock.clone())).await;
    let config = Config {
        data: harnx_core::config_data::ConfigData {
            stream: false,
            ..Default::default()
        },
        ..Default::default()
    };
    let config = Arc::new(ConfigLock::new(config));
    let prompt = "system instructions ".repeat(2000);
    let agent = AgentConfig::from_prompt(&prompt);
    assert_eq!(
        fetch_summary(source(), agent, &config).await.unwrap(),
        "Summary."
    );
    let history = mock.conversation_history();
    assert_eq!(history.conversation_history.len(), 1);
    assert_eq!(
        history.conversation_history[0].messages[0]
            .content
            .to_text(),
        prompt
    );
}

#[test]
fn merged_system_and_user_request_fits_the_same_estimator_budget() {
    let config = Arc::new(ConfigLock::new(Config::default()));
    let data = serde_json::from_value(serde_json::json!({
        "name": "no-system", "max_input_tokens": 2000, "no_system_message": true,
        "system_prompt_prefix": ["model-specific instructions"]
    }))
    .unwrap();
    let model = Model::from_config("mock", &[data]).remove(0);
    let mut input = input(&"system instructions ".repeat(90));
    let data = prepare_request(&mut input, &config, &model, &source()).unwrap();
    assert_eq!(data.messages.len(), 1);
    assert!(model.messages_tokens(&data.messages) + REQUEST_OVERHEAD <= input_budget(&model));
    let text = data.messages[0].content.to_text();
    assert!(text.contains("system instructions"));
    assert!(text.contains("latest question") && text.contains("latest answer"));
}

#[test]
fn request_budget_deducts_selected_tool_definitions() {
    use harnx_core::tool::{ToolDeclaration, Tools};
    let declaration: ToolDeclaration = serde_json::from_value(serde_json::json!({
        "name": "history_read",
        "description": "Read earlier messages from the session log. ".repeat(80),
        "parameters": {"type": "object", "properties": {"offset": {"type": "integer"}}}
    }))
    .unwrap();
    let config = Arc::new(ConfigLock::new(Config {
        tools: Tools::init_from_mcp(Some(vec![declaration])),
        ..Default::default()
    }));
    let model = model(Some(3000));
    let mut input = input("summarize");
    input.agent.set_model(model.clone());
    let source = source();
    let plain = prepare_request(&mut input, &config, &model, &source).unwrap();
    assert!(plain.functions.is_none());

    input.agent.set_use_tools(Some(vec!["history_read".into()]));
    let with_tools = prepare_request(&mut input, &config, &model, &source).unwrap();
    let functions = with_tools
        .functions
        .as_ref()
        .expect("summarizer tool selected");
    assert_eq!(functions.len(), 1);
    assert_eq!(functions[0].name, "history_read");
    let definitions = estimate_token_length(&serde_json::to_string(functions).unwrap());
    assert!(definitions > 500);
    assert!(
        model.messages_tokens(&plain.messages) + REQUEST_OVERHEAD + definitions
            > input_budget(&model),
        "fixture must overflow if definitions aren't deducted"
    );
    assert!(
        model.messages_tokens(&with_tools.messages) + REQUEST_OVERHEAD + definitions
            <= input_budget(&model)
    );
    let text = with_tools.messages[1].content.to_text();
    assert!(text.contains("chars omitted"));
    assert!(text.contains("latest question") && text.contains("latest answer"));
}

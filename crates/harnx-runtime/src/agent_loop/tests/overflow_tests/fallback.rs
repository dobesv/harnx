use super::*;
use harnx_core::model::Model;
use harnx_engine::retry::{call_with_retry_and_fallback_custom, TurnContext};

#[tokio::test]
async fn primary_overflow_then_auth_fallback_compacts_and_retries_primary() {
    let config = config_with_old_history();
    let mock = Arc::new(
        MockClient::builder()
            .add_turn(MockTurn::with_text("Summary."))
            .build(),
    );
    let _mock = crate::client::TestStateGuard::new(Some(mock.clone())).await;
    let mut ctx = context(config.clone());
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    ctx.call_fn = Some(overflow_then_auth(calls.clone()));
    let mut input = crate::config::input::from_str(&config, "Current question", None);
    input
        .agent_mut()
        .set_model(Model::new("claude", "claude-sonnet"));
    input
        .agent_mut()
        .set_model_fallbacks(vec!["gemini:gemini-2.5-pro".into()]);
    let sink = Arc::new(CollectingSink::default());
    let result = harnx_core::sink::with_agent_event_sink(sink.clone(), run_agent_loop(&ctx, input))
        .await
        .unwrap();
    assert!(matches!(result, LoopResult::Completed));
    assert_eq!(
        *calls.lock().unwrap(),
        [
            "claude:claude-sonnet",
            "gemini:gemini-2.5-pro",
            "claude:claude-sonnet"
        ]
    );
    assert_eq!(
        mock.conversation_history().conversation_history.len(),
        1,
        "exactly one real compactor call"
    );
    assert_compaction_lifecycle(&sink);
}

fn overflow_then_auth(seen: Arc<std::sync::Mutex<Vec<String>>>) -> crate::agent_loop::AgentCallFn {
    Arc::new(move |input, config, abort| {
        let seen = seen.clone();
        let config = config.clone();
        Box::pin(async move {
            let turn = turn_context();
            call_with_retry_and_fallback_custom(input, &turn, abort, move |input, client, _| {
                let seen = seen.clone();
                let config = config.clone();
                Box::pin(async move {
                    let id = client.model().id();
                    let mut calls = seen.lock().unwrap();
                    calls.push(id.clone());
                    if id.starts_with("gemini:") {
                        return Err(LlmError {
                            status: 401,
                            message: "no credentials".into(),
                            retry_after: None,
                        }
                        .into());
                    }
                    if calls.len() == 1 {
                        return Err(LlmError {
                            status: 400,
                            message: "context_length_exceeded".into(),
                            retry_after: None,
                        }
                        .into());
                    }
                    let request = texts(&crate::config::input::build_messages(input, &config)?);
                    assert!(request.iter().any(|text| text.contains("Summary.")));
                    assert!(!request.iter().any(|text| text.contains("archived context")));
                    Ok(("done".into(), None, vec![], Default::default()))
                })
            })
            .await
        })
    })
}

fn turn_context() -> TurnContext {
    let clients = ["claude", "gemini"]
        .into_iter()
        .map(|provider| {
            let mut client: crate::client::ClientConfig =
                serde_yaml::from_str(&format!("type: {provider}\napi_key: test\n")).unwrap();
            client.set_name(provider.into());
            client
        })
        .collect();
    TurnContext {
        default_model_id: "claude:claude-sonnet".into(),
        clients,
        model_cooldowns: Default::default(),
        warn_fn: Arc::new(|_| {}),
        event_fn: Arc::new(|_| {}),
        init_client_fn: Arc::new(|_, model| {
            Ok(Box::new(MockClient::builder().model(model.clone()).build()))
        }),
        select_model_fn: Arc::new(|input, model| input.agent_mut().set_model(model.clone())),
    }
}

//! Retry/fallback fixture records real request construction across provider attempts.
use super::*;
use harnx_core::model::Model;
use harnx_engine::retry::TurnContext;

pub(super) fn setup_retry_fallback_input(
    config: &GlobalConfig,
    origin: chrono::DateTime<chrono::Utc>,
) -> Result<(crate::config::input::Input, TimingReceipt)> {
    let mut agent = harnx_core::agent_config::AgentConfig::from_markdown(
        "timing-retry",
        "---\nretry:\n  attempts: 2\n  initial_delay_ms: 0\n  max_delay_ms: 0\n---\ntest",
    )?;
    agent.set_model(Model::new("claude", "claude-sonnet"));
    agent.set_model_fallbacks(vec!["gemini:gemini-2.5-pro".into()]);
    let mut input = crate::config::input::from_str(
        config,
        "opening prompt",
        Some(crate::config::Agent::new(agent)),
    );
    input.with_session = true;
    crate::config::session_timing::start_run(config, origin);
    prepare_at(&mut input, config, origin)?;
    let receipt = input.timing_context.as_ref().unwrap().receipt.clone();
    Ok((input, receipt))
}

pub(super) fn make_retry_fallback_turn_context() -> TurnContext {
    TurnContext {
        default_model_id: "claude:claude-sonnet".into(),
        clients: ["claude", "gemini"]
            .into_iter()
            .map(|provider| {
                let mut client: ClientConfig =
                    serde_yaml::from_str(&format!("type: {provider}\napi_key: test-key\n"))
                        .unwrap();
                client.set_name(provider.into());
                client
            })
            .collect(),
        model_cooldowns: Arc::new(parking_lot::Mutex::new(Default::default())),
        warn_fn: Arc::new(|_| {}),
        event_fn: Arc::new(|_| {}),
        init_client_fn: Arc::new(|_, model| {
            Ok(Box::new(
                crate::test_utils::MockClient::builder()
                    .model(model.clone())
                    .build(),
            ))
        }),
        select_model_fn: Arc::new(|input, model| input.agent_mut().set_model(model.clone())),
    }
}

pub(super) async fn execute_retry_and_fallback_calls(
    input: &mut crate::config::input::Input,
    ctx: &TurnContext,
    config: &GlobalConfig,
) -> Result<Vec<(String, Vec<Message>)>> {
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = requests.clone();
    let request_config = config.clone();
    harnx_engine::retry::call_with_retry_and_fallback_custom(
        input,
        ctx,
        create_abort_signal(),
        move |input, client, _| {
            let captured = captured.clone();
            let config = request_config.clone();
            let provider = client.model().client_name().to_string();
            Box::pin(async move {
                let messages = crate::config::input::prepare_completion_data(
                    input,
                    &config,
                    client.model(),
                    false,
                    client,
                )?
                .messages;
                captured.lock().unwrap().push((provider.clone(), messages));
                if provider == "claude" {
                    Err(crate::client::LlmError {
                        status: 503,
                        message: "unavailable".into(),
                        retry_after: None,
                    }
                    .into())
                } else {
                    Ok(("answer".into(), None, vec![], Default::default()))
                }
            })
        },
    )
    .await?;
    let res = requests.lock().unwrap().clone();
    Ok(res)
}

fn assert_retry_fallback_provider_sequence(requests: &[(String, Vec<Message>)]) {
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests
            .iter()
            .map(|(provider, _)| provider.as_str())
            .collect::<Vec<_>>(),
        ["claude", "claude", "gemini"]
    );
}

fn assert_retry_fallback_wire_receipts(
    requests: &[(String, Vec<Message>)],
    receipt: &TimingReceipt,
) {
    for (_, messages) in requests.iter() {
        assert_eq!(timing_messages(messages).len(), 1);
        assert_eq!(
            harnx_core::timing_identity::TimingReceipt::from_notice_line(
                &timing_messages(messages)[0].content.to_text()
            ),
            Some(receipt.clone())
        );
    }
}

fn assert_retry_fallback_persistence(
    receipt: &TimingReceipt,
    input: &crate::config::input::Input,
    log: &PromptLog,
) {
    assert_eq!(input.timing_context.as_ref().unwrap().receipt, *receipt);
    assert_eq!(timing_messages(&log.messages()).len(), 1);
}

pub(super) fn assert_retry_fallback_requests(
    requests: &[(String, Vec<Message>)],
    receipt: &TimingReceipt,
    input: &crate::config::input::Input,
    log: &PromptLog,
) {
    assert_retry_fallback_provider_sequence(requests);
    assert_retry_fallback_wire_receipts(requests, receipt);
    assert_retry_fallback_persistence(receipt, input, log);
}

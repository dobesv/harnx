//! Exercise the shipped chains through the production retry loop without network calls.

use anyhow::{ensure, Context, Result};
use harnx_core::{agent_config::AgentConfig, input::Input};
use harnx_engine::retry::{call_with_retry_and_fallback_custom, TurnContext};
use harnx_runtime::{
    client::{retrieve_model, ModelType},
    config::Config,
};
use std::{collections::HashSet, sync::Arc};

const PROVIDERS: [&str; 5] = ["gemini", "claude", "codex", "openai", "bedrock"];

fn provider(model_id: &str) -> &str {
    model_id
        .split_once(':')
        .expect("qualified model ID")
        .0
        .rsplit('/')
        .next()
        .expect("client stem")
}

pub fn check_chain(config: &Config, model_ids: &[&str]) -> Result<()> {
    let providers: HashSet<_> = model_ids.iter().map(|id| provider(id)).collect();
    ensure!(
        PROVIDERS.iter().all(|p| providers.contains(p)),
        "incomplete provider coverage: {model_ids:?}"
    );
    ensure!(
        model_ids.iter().collect::<HashSet<_>>().len() == model_ids.len(),
        "duplicate fallback"
    );
    for (index, id) in model_ids.iter().enumerate() {
        let model = retrieve_model(&config.clients, id, ModelType::Chat)?;
        if model.real_name().contains("claude-opus") {
            ensure!(
                model.real_name() == "claude-opus-4-8",
                "unapproved Opus model: {id}"
            );
        }
        if provider(id) == "bedrock" {
            ensure!(
                !model.real_name().contains("anthropic"),
                "Anthropic via Bedrock: {id}"
            );
        }
        if provider(id) == "openai" {
            let previous = index.checked_sub(1).map(|i| model_ids[i]);
            let expected = id.replace("/openai:", "/codex:");
            ensure!(
                previous == Some(expected.as_str()),
                "OpenAI must immediately follow matching Codex: {id}"
            );
        }
    }
    Ok(())
}

fn turn_context(config: &Config) -> TurnContext {
    TurnContext {
        default_model_id: String::new(),
        clients: config.clients.clone(),
        model_cooldowns: Default::default(),
        warn_fn: Arc::new(|_| {}),
        event_fn: Arc::new(|_| {}),
        init_client_fn: Arc::new(harnx_client::init_client),
        select_model_fn: Arc::new(|input, model| input.agent.set_model(model.clone())),
    }
}

pub async fn check_single_provider_fallbacks(config: &Config, agent: &AgentConfig) -> Result<()> {
    let primary_id = agent
        .model_id()
        .context("agent must declare a primary model")?;
    let primary = retrieve_model(&config.clients, primary_id, ModelType::Chat)?;
    // Each configured provider must be independently sufficient, even when all
    // preceding clients reject their credentials. Also check Codex + API-key ordering.
    for available in [
        "gemini",
        "claude",
        "codex",
        "openai",
        "bedrock",
        "codex+openai",
    ] {
        let mut input = Input::new("hello".into(), ("hello".into(), vec![]), agent.clone());
        input.agent.set_resolved_model(primary.clone());
        let context = turn_context(config);
        let result = call_with_retry_and_fallback_custom(
            &mut input,
            &context,
            harnx_core::abort::create_abort_signal(),
            move |_, client, _| {
                Box::pin(async move {
                    let id = client.model().id();
                    if available.split('+').any(|p| p == provider(&id)) {
                        Ok((id, None, vec![], Default::default()))
                    } else {
                        Err(harnx_core::error::LlmError {
                            status: 401,
                            message: "test: credentials unavailable".into(),
                            retry_after: None,
                        }
                        .into())
                    }
                })
            },
        )
        .await
        .with_context(|| format!("only {available} is available"))?;
        let expected = if available == "codex+openai" {
            "codex"
        } else {
            available
        };
        ensure!(
            provider(&result.0) == expected,
            "wrong provider selected: {}",
            result.0
        );
        ensure!(
            input.agent.model().id() == result.0,
            "selected model was not saved"
        );
    }
    Ok(())
}

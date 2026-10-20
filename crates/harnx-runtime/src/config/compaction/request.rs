//! Budget the one-shot compactor after retry/fallback has selected its model.

use super::completion::CompletionCall;
use super::Transcript;
use crate::client::ChatCompletionsData;
use crate::config::{GlobalConfig, Input};
use anyhow::Result;
use harnx_core::agent_config::AgentConfig;
use harnx_core::model::Model;
use harnx_core::session::COMPRESS_THRESHOLD_MODEL_LIMIT_PERCENT;
use harnx_core::text::{estimate_token_length, strip_think_tag};

/// Covers message framing and small provider/retry additions, not exact tokenization.
const REQUEST_OVERHEAD: usize = 1024;
const DEFAULT_INPUT_BUDGET: usize = 150_000;

fn input_budget(model: &Model) -> usize {
    model
        .max_input_tokens()
        .map_or(DEFAULT_INPUT_BUDGET, |max| {
            // Share the compaction trigger's headroom, not its default threshold.
            // Quotient/remainder also preserves the full range of model metadata.
            let percent = COMPRESS_THRESHOLD_MODEL_LIMIT_PERCENT;
            max / 100 * percent + max % 100 * percent / 100
        })
}

fn request_messages(
    input: &Input,
    config: &GlobalConfig,
    model: &Model,
) -> Result<Vec<harnx_core::message::Message>> {
    let mut messages = crate::config::input::build_messages(input, config)?;
    crate::client::patch_messages(&mut messages, model);
    Ok(messages)
}

fn prepare_request(
    input: &mut Input,
    config: &GlobalConfig,
    model: &Model,
    transcript: &Transcript,
) -> Result<ChatCompletionsData> {
    let stream = crate::config::input::stream(input, config);
    let functions = config.read().select_tools(input.agent());
    input.resolved_tools = functions.clone();
    input.set_text(String::new());
    let base = request_messages(input, config, model)?;
    let definitions = functions.as_ref().map_or(0, |tools| {
        estimate_token_length(&serde_json::to_string(tools).unwrap_or_default())
    });
    let overhead = REQUEST_OVERHEAD.saturating_add(definitions);
    let limit = input_budget(model);
    let mut budget = limit.saturating_sub(model.messages_tokens(&base).saturating_add(overhead));
    let mut text = transcript.fit(budget);
    let messages = loop {
        input.set_text(text.clone());
        let messages = request_messages(input, config, model)?;
        let estimated = model.messages_tokens(&messages).saturating_add(overhead);
        if estimated <= limit {
            break messages;
        }
        // Recheck the assembled request: providers without a system role merge
        // prompt and transcript, so estimator rounding can differ from their sum.
        budget = budget.saturating_sub(estimated - limit);
        let reduced = transcript.fit(budget);
        if reduced == text && budget == 0 {
            log::warn!(
                "compaction request for {} cannot fit its estimated input budget: {} > {}; \
                 sending minimum transcript without rewriting the summarizer system prompt",
                model.id(),
                estimated,
                limit
            );
            break messages;
        }
        text = reduced;
    };
    // Compaction owns size reduction. In the irreducible configuration case,
    // send the minimum request instead of failing the ordinary input-size guard.
    // All content is flattened text; don't re-expand archived image attachments.
    Ok(ChatCompletionsData {
        messages,
        temperature: input.agent().temperature(),
        top_p: input.agent().top_p(),
        functions,
        stream,
        attachments_dir: None,
    })
}

pub(crate) async fn fetch_summary(
    transcript: Transcript,
    agent: AgentConfig,
    config: &GlobalConfig,
) -> Result<String> {
    let transcript = std::sync::Arc::new(transcript);
    let mut input = Input::new(String::new(), (String::new(), vec![]), agent);
    input.with_session = false;
    input.with_agent = true;
    let (text, _, _, _) = crate::client::retry::call_with_retry_and_fallback_custom(
        &mut input,
        config,
        crate::utils::create_abort_signal(),
        move |input, client, config, abort| {
            let transcript = transcript.clone();
            Box::pin(async move {
                let data = prepare_request(input, config, client.model(), &transcript)?;
                CompletionCall::new(input, client, config)
                    .send(data, abort)
                    .await
            })
        },
    )
    .await?;
    Ok(strip_think_tag(&text).into_owned())
}

#[cfg(test)]
#[path = "request_tests.rs"]
mod tests;

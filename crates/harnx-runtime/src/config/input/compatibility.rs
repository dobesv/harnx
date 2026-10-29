//! Reject input a selected model cannot carry before request patches can lose it.

use anyhow::Result;
use harnx_core::{
    error::{LlmError, ModelFallbackReason},
    message::Message,
    model::{request_image_count, Model},
    tool::ToolDeclaration,
};

pub(super) fn guard_images(model: &Model, messages: &[Message]) -> Result<()> {
    if !model.supports_vision()
        && messages
            .iter()
            .any(|message| request_image_count(&message.content) > 0)
    {
        return Err(
            anyhow::anyhow!("Model '{}' does not support images", model.id())
                .context(ModelFallbackReason::IncompatibleInput),
        );
    }
    Ok(())
}

pub(super) fn guard_request_size(
    model: &Model,
    messages: &[Message],
    tools: &[ToolDeclaration],
) -> Result<()> {
    // Use the same conservative estimate as mid-turn compaction, including tool
    // schemas and images, while retaining the old per-message overhead guard.
    let tokens = model
        .estimate_request_tokens(messages, tools)
        .max(model.total_tokens(messages).saturating_add(2));
    if let Some(limit) = model.max_input_tokens().filter(|limit| tokens >= *limit) {
        return Err(LlmError {
            status: 400,
            message: format!(
                "context_length_exceeded: estimated request size {tokens} meets or exceeds '{}' input limit {limit}",
                model.id()
            ),
            retry_after: None,
        }.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests;

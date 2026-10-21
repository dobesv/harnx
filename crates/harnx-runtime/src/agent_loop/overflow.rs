//! Overflow recovery: one forced compaction, one retry.
//!
//! When a provider rejects a request with a context-overflow error, the shared
//! agent loop calls this module instead of failing immediately. It forces
//! compaction (ignoring threshold), then retries the request once. The retry
//! uses the same `Input` with its request-only patch intact; the prepared
//! input suffix is preserved across compaction.
//!
//! First-request handling: NATS loads selected input before the turn starts,
//! so overflow on the first request preserves those rows. Local mode finds
//! the prompt via `prompt_start`. New user input is never appended twice.
//!
//! Recovery failure says "compaction was tried" in the error chain.
//! If there's nothing older to compact, recovery fails rather than summarizing
//! the new prompt away (C2 invariant).
use super::*;
use anyhow::Context;

pub(super) async fn call(ctx: &AgentLoopContext, input: &mut Input) -> AgentModelResult {
    let error = match Box::pin(committed_agent_model(ctx, input)).await {
        Ok(output) => return Ok(output),
        Err(error) => error,
    };
    let overflow = harnx_engine::retry::find_llm_error(&error)
        .is_some_and(|error| error.is_context_overflow());
    if !input.with_session() {
        return Err(error);
    }
    if !overflow || ctx.config.read().session.is_none() {
        return Err(error);
    }
    recover(ctx, input).await.with_context(|| {
        format!("Context overflow: compaction was tried but failed (original error: {error:#})")
    })?;
    Box::pin(committed_agent_model(ctx, input))
        .await
        .context("Model request failed after context compaction was tried (one recovery retry)")
}

async fn recover(ctx: &AgentLoopContext, input: &mut Input) -> Result<()> {
    ctx.check_generation("overflow-compaction")?;
    wait_for_session_compaction(&ctx.config).await;
    let prepared = PreparedInput::capture(&ctx.config, input)?;
    compaction::compact_preserving(ctx, Some(prepared.protected_from)).await?;
    ctx.check_generation("overflow-compacted")?;
    prepared.rebase(&ctx.config, input)?;
    enforce_token_budget(ctx)
}

struct PreparedInput {
    session_id: String,
    appended_count: usize,
    protected_from: usize,
}

impl PreparedInput {
    fn capture(config: &GlobalConfig, input: &Input) -> Result<Self> {
        let guard = config.read();
        let session = guard.session.as_ref().context("No session")?;
        let start = input.session_input_start.unwrap_or(session.messages.len());
        anyhow::ensure!(
            start <= session.messages.len(),
            "session history changed after input persistence"
        );
        let mut protected_from = start;
        if input.tool_calls.is_none() {
            // NATS already loaded its selected user rows before preparation. In
            // local mode this prompt instead belongs to the newly appended suffix.
            if let Some(prompt) = prompt_start(&session.messages, input) {
                protected_from = protected_from.min(prompt);
            }
        }
        Ok(Self {
            session_id: session.id.clone(),
            appended_count: session.messages.len() - start,
            protected_from,
        })
    }

    fn rebase(self, config: &GlobalConfig, input: &mut Input) -> Result<()> {
        let guard = config.read();
        let session = guard
            .session
            .as_ref()
            .context("No session after compaction")?;
        anyhow::ensure!(
            session.id == self.session_id,
            "active session changed during overflow recovery"
        );
        // Keep request-only patches/injected text on Input. Exclude their durable
        // unpatched rows from the retry prefix instead of appending input again.
        input.session_input_start = Some(
            session
                .messages
                .len()
                .checked_sub(self.appended_count)
                .context("prepared input was lost during overflow compaction")?,
        );
        Ok(())
    }
}

fn prompt_start(messages: &[harnx_core::message::Message], input: &Input) -> Option<usize> {
    let selected = messages.iter().position(|message| {
        input
            .durable_user_messages
            .iter()
            .any(|selected| message.same_identity(selected))
    });
    selected.or_else(|| messages.iter().rposition(crate::config::is_turn_prompt))
}

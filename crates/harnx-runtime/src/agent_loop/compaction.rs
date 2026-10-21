//! Preflight compaction runs once per tool round, before preparing the next model request.
//!
//! The shared agent loop calls `preflight` from `prepare_round_input`, which runs
//! after tool results are merged and before `before_chat_completion` persists the
//! input cursor. Compaction never fires for fresh user input (`tool_calls.is_none()`)
//! or sessionless inputs. This ordering matters: compacting after input preparation
//! would invalidate `session_input_start`.
use super::*;
use harnx_core::event::{AgentEvent, SessionEvent};

pub(super) async fn preflight(ctx: &AgentLoopContext, input: &mut Input) -> Result<()> {
    if input.tool_calls.is_none() || !input.with_session() {
        return Ok(());
    }
    ctx.check_generation("before-compaction")?;
    if request_needs_compaction(&ctx.config, input)? {
        if let Err(error) = compact_in_turn(ctx).await {
            ctx.check_generation("compaction-failed")?;
            // The estimate has headroom; a failed summarizer doesn't prove the
            // main request won't fit. Provider overflow recovery stays strict.
            log::warn!(
                "Preflight compaction failed; continuing with uncompacted history: {error:#}"
            );
        }
        ctx.check_generation("after-compaction")?;
    }
    Ok(())
}

fn request_needs_compaction(config: &GlobalConfig, input: &mut Input) -> Result<bool> {
    let threshold = {
        let guard = config.read();
        let Some(session) = guard.session.as_ref() else {
            return Ok(false);
        };
        input.resolved_tools = guard.select_tools(input.agent());
        harnx_core::session::effective_compress_threshold(
            session.model(),
            session.compress_threshold,
            guard.compress_threshold,
        )
    };
    if threshold == 0 {
        return Ok(false);
    }
    let messages = crate::config::input::build_messages(input, config)?;
    Ok(input.agent().model().estimate_request_tokens(
        &messages,
        input.resolved_tools.as_deref().unwrap_or_default(),
    ) > threshold)
}

/// Await the ordinary compaction path and emit the same lifecycle as maintenance.
/// Overflow recovery uses `compact_preserving` for already prepared input.
pub(super) async fn compact_in_turn(ctx: &AgentLoopContext) -> Result<()> {
    compact_preserving(ctx, None).await
}

pub(super) async fn compact_preserving(
    ctx: &AgentLoopContext,
    protected_from: Option<usize>,
) -> Result<()> {
    let Some(flight) = CompactionFlight::begin(&ctx.config) else {
        anyhow::ensure!(
            protected_from.is_none(),
            "session compaction already in progress during overflow recovery"
        );
        return Ok(());
    };
    let started = std::time::Instant::now();
    let result = tokio::select! {
        biased;
        _ = harnx_core::abort::wait_abort_signal(&ctx.abort_signal) => Err(anyhow::anyhow!("interrupted by user")),
        result = Box::pin(Config::compact_session_preserving(&ctx.config, protected_from)) => result,
    };
    Config::handle_compaction_result(&result, started, flight.message_count, None);
    result
}

struct CompactionFlight<'a> {
    config: &'a GlobalConfig,
    session_id: String,
    message_count: usize,
}

impl<'a> CompactionFlight<'a> {
    fn begin(config: &'a GlobalConfig) -> Option<Self> {
        let mut guard = config.write();
        let session = guard.session.as_mut()?;
        if session.compressing() {
            return None;
        }
        session.set_compressing(true);
        let flight = Self {
            config,
            session_id: session.id.clone(),
            message_count: session.messages.len(),
        };
        drop(guard);
        harnx_core::sink::emit_agent_event(AgentEvent::Session(SessionEvent::CompactingStarted {
            compaction_id: None,
        }));
        Some(flight)
    }
}

impl Drop for CompactionFlight<'_> {
    fn drop(&mut self) {
        let mut guard = self.config.write();
        if let Some(session) = guard.session.as_mut().filter(|s| s.id == self.session_id) {
            session.set_compressing(false);
        }
    }
}

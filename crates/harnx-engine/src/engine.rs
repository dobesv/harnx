//! Engine implementation. `Engine::run_turn` spawns a task that emits
//! `AgentEvent` values through both `ctx.sink` (push) and the returned
//! stream (pull). See `docs/superpowers/specs/2026-04-19-monorepo-refactor-design.md`
//! for the full protocol.

use std::sync::Arc;

use futures_util::Stream;
use harnx_core::context::SessionCtx;
use harnx_core::event::{AgentEvent, NoticeEvent, TurnEvent, TurnOutcome};
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::input::EngineInput;

/// Skeleton for the harnx engine. Currently holds no state; the real
/// engine will carry a client registry and a tool registry. Keep the
/// constructor permissive so tests and future wiring can both use it.
#[derive(Debug, Default, Clone, Copy)]
pub struct Engine {
    // Future fields (commented placeholders, not yet added):
    //   client_registry: ClientRegistry,
    //   tool_registry: ToolRegistry,
}

impl Engine {
    /// Construct a new engine. No configuration is required at this
    /// stage; future versions will take a `&GlobalConfig` (or a
    /// narrowed trait) to build the client and tool registries.
    pub fn new() -> Self {
        Self::default()
    }

    /// Execute one turn. Emits events through both the returned stream
    /// and `ctx.sink`. The stream terminates after `Turn(Ended)` or an
    /// abort-driven `Notice(Warning)` + `Turn(Ended)`.
    ///
    /// Current behavior (scaffold): emits `Turn(Started)` → checks
    /// `ctx.abort`, emitting `Notice(Warning("interrupted"))` if set →
    /// emits `Turn(Ended { outcome: default })`. Future plans insert
    /// the real LLM call, retry/fallback, tool eval loop, and stop-hook
    /// resume between Started and Ended.
    pub fn run_turn(
        &self,
        ctx: Arc<SessionCtx>,
        _input: EngineInput,
    ) -> impl Stream<Item = AgentEvent> + Send + 'static {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        let ctx_for_task = Arc::clone(&ctx);
        tokio::spawn(async move {
            let emit = |event: AgentEvent| {
                ctx_for_task.sink.emit(event.clone(), None);
                let _ = tx.send(event);
            };

            emit(AgentEvent::Turn(TurnEvent::Started));

            if ctx_for_task.abort.aborted() {
                emit(AgentEvent::Notice(NoticeEvent::Warning(
                    "interrupted".to_string(),
                )));
            }

            emit(AgentEvent::Turn(TurnEvent::Ended {
                outcome: TurnOutcome::default(),
            }));
            // tx is dropped when the task exits, closing the stream.
        });
        UnboundedReceiverStream::new(rx)
    }
}

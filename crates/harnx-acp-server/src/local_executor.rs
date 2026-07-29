use std::sync::Arc;

use agent_client_protocol as acp;
use harnx_core::event::AgentEventSink;
use harnx_runtime::{config::GlobalConfig, utils::AbortSignal};

pub(super) struct LocalTurnParams<'a> {
    pub agent_name: &'a str,
    pub session_config: &'a GlobalConfig,
    pub prompt_text: &'a str,
    pub abort_signal: AbortSignal,
    pub sink: Arc<dyn AgentEventSink>,
}

/// Execute a worker-owned ACP backend turn in-process.
///
/// Phase 1 keeps tools and sub-agents inside the worker process tree. Scoping
/// the supplied sink preserves nested `AgentEvent::SubAgent` forwarding into
/// the parent worker's advisory stream, while the shared abort signal preserves
/// ACP cancellation propagation.
pub(super) async fn run_local_turn(params: LocalTurnParams<'_>) -> anyhow::Result<()> {
    let mut agent = params
        .session_config
        .read()
        .retrieve_agent(params.agent_name)
        .map_err(|e| acp::Error::new(-32603, format!("Failed to retrieve agent: {e}")))?;
    harnx_runtime::config::agent::resolve_variables(&mut agent)
        .map_err(|e| acp::Error::new(-32603, format!("Failed to resolve agent: {e}")))?;
    let mut input =
        harnx_runtime::config::input::from_str(params.session_config, params.prompt_text, None);
    harnx_runtime::config::input::set_agent(&mut input, params.session_config, agent.into_config());
    let loop_ctx = harnx_session::build_context(
        params.session_config.clone(),
        None,
        params.abort_signal,
        None,
        None,
    );
    harnx_core::sink::with_agent_event_sink(params.sink, async {
        // Keep large recursive agent-loop future off caller's stack. ACP tests
        // construct several prompt futures in one `join!`; embedding each loop
        // future inline overflows default nextest worker stack before polling.
        Box::pin(harnx_runtime::run_agent_loop_with_local_handoff(
            &loop_ctx, input,
        ))
        .await
        .map(|_| ())
    })
    .await
}

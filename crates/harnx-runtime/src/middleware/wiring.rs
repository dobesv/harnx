use super::{Chain, Entry, Invocation};
use crate::config::Config;
use anyhow::Result;
use harnx_core::tool::ToolError;
use harnx_engine::tool::{ToolEvalContext, ToolMiddlewareFn};
use parking_lot::Mutex;
use std::sync::Arc;
use tracing::Instrument;

pub(crate) fn install(ctx: &mut ToolEvalContext, config: &Config) {
    ctx.middleware_fn = resolve(config, ctx.current_agent_package.as_deref())
        .map(configured_callback)
        .unwrap_or_else(failed_callback);
}

fn configured_callback(chain: Chain) -> Option<Arc<ToolMiddlewareFn>> {
    let configured = !chain.entries.is_empty();
    configured.then(|| callback(Arc::new(chain)))
}

fn failed_callback(error: anyhow::Error) -> Option<Arc<ToolMiddlewareFn>> {
    // Builders predate fallible middleware configuration. Never silently
    // dispatch an unprotected call when resolution fails on a refreshed catalog.
    let message = format!("middleware configuration: {error:#}");
    Some(Arc::new(move |_, _, _| {
        let message = message.clone();
        Box::pin(async move { Err(ToolError::Recoverable(anyhow::anyhow!(message))) })
    }))
}
fn resolve(config: &Config, package: Option<&str>) -> Result<Chain> {
    let chain = config.resolved_middleware()?;
    let entries = chain
        .entries
        .into_iter()
        .map(|definition| {
            let tools = config
                .select_tools_for_package(&definition.tools, package)
                .into_iter()
                .filter(|tool| crate::config::is_bash_inner_tool_allowed(&tool.name))
                .collect();
            Arc::new(Entry { definition, tools })
        })
        .collect();
    Ok(Chain {
        entries,
        version: chain.version,
        session_id: config
            .session
            .as_ref()
            .map(|session| session.id().to_string()),
        session_storage_key: config
            .session
            .as_ref()
            .map(|session| session.storage_key())
            .unwrap_or_default(),
        agent: config.tool_selection_agent().name().to_string(),
    })
}

fn callback(chain: Arc<Chain>) -> Arc<ToolMiddlewareFn> {
    Arc::new(move |prepared, origin, leaf| {
        let chain = chain.clone();
        Box::pin(async move {
            let entries = chain
                .entries
                .iter()
                .filter_map(
                    |entry| match entry.definition.matcher.is_match(&prepared.call.name) {
                        Ok(true) => Some(Ok(entry.clone())),
                        Ok(false) => None,
                        Err(error) => Some(Err(ToolError::Recoverable(error.into()))),
                    },
                )
                .collect::<Result<Vec<_>, _>>()?;
            let arguments = prepared.arguments.clone();
            let state = super::resume::CheckpointState::new(
                chain.session_storage_key.clone(),
                &prepared,
                chain.version.clone(),
            );
            let resume =
                super::checkpoint::ResumeState::new(state).map_err(ToolError::Recoverable)?;
            let invocation = Arc::new(Invocation {
                chain,
                prepared,
                entries,
                leaf,
                terminal: Arc::new(Mutex::new(None)),
                origin,
                resume: Arc::new(Mutex::new(resume)),
                attempt: crate::interaction_checkpoint::attempt::current(),
            });
            let outcome = invocation
                .clone()
                .run_at(0, arguments, vec![], vec![])
                .instrument(tracing::info_span!("tool_middleware_chain", ?origin))
                .await;
            let terminal = invocation.terminal.lock().take();
            let mut outcome = terminal.unwrap_or(outcome)?;
            merge_frame_checkpoint(&mut outcome, &invocation)?;
            Ok(outcome)
        })
    })
}

/// A tool challenge's checkpoint (T4.3) carries the tool's request state;
/// the chain's frames carry the middleware stash. Keep both: the leaf built
/// the tool state, the invocation owns the frames. Middleware-produced
/// suspensions already carry their frames and pass through unchanged.
fn merge_frame_checkpoint(
    outcome: &mut harnx_engine::tool::ToolMiddlewareOutcome,
    invocation: &Arc<Invocation>,
) -> Result<(), ToolError> {
    if let harnx_engine::tool::ToolMiddlewareOutcome::InputRequired { checkpoint, .. } = outcome {
        let tool_request_state = checkpoint
            .state
            .get("tool_request_state")
            .cloned()
            .filter(|state| !state.is_null());
        *checkpoint = invocation
            .resume
            .lock()
            .checkpoint()
            .map_err(ToolError::Recoverable)?;
        if let Some(state) = tool_request_state {
            checkpoint.state["tool_request_state"] = state;
        }
    }
    Ok(())
}

//! In-process script middleware. Each attempt owns its frames and private refs.

mod bash;
mod checkpoint;
mod completion;
mod confirm;
pub(crate) mod conversion;
mod effects;
mod envelope;
pub(crate) mod fetch;
mod fetch_net;
mod frame;
mod interact;
pub(crate) mod python;
mod resume;
mod script;

pub(crate) use resume::{checkpoint_tool_request_state, current_tool_input, with_tool_input};
pub use resume::{with_resume, AcceptedInteraction, MiddlewareResume, ToolInputResume};
pub(crate) use tool_challenge::{tool_challenge_outcome, ToolChallengeContext};
mod tool_challenge;
pub(crate) mod tools;
mod trace;
mod wiring;

pub(crate) use wiring::install;

use effects::{DispatchGuard, Effects};
use frame::{Frame, Step};
use harnx_core::{
    middleware::ResolvedMiddleware,
    tool::{ToolDeclaration, ToolError},
};
use harnx_engine::tool::{
    PreparedToolCall, ToolCallNext, ToolMiddlewareFuture, ToolMiddlewareOutcome,
};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::Instrument;

struct Entry {
    definition: ResolvedMiddleware,
    tools: Vec<ToolDeclaration>,
}

struct Chain {
    entries: Vec<Arc<Entry>>,
    version: String,
    session_id: Option<String>,
    session_storage_key: String,
    agent: String,
}

struct Invocation {
    chain: Arc<Chain>,
    prepared: PreparedToolCall,
    entries: Vec<Arc<Entry>>,
    leaf: Arc<ToolCallNext>,
    terminal: Arc<Mutex<Option<Result<ToolMiddlewareOutcome, ToolError>>>>,
    origin: harnx_engine::tool::ToolMiddlewareOrigin,
    resume: Arc<Mutex<checkpoint::ResumeState>>,
    attempt: Option<crate::interaction_checkpoint::attempt::Attempt>,
}

impl Invocation {
    fn run_at(
        self: Arc<Self>,
        index: usize,
        arguments: Value,
        path: Vec<Step>,
        ancestors: Vec<Arc<Effects>>,
    ) -> ToolMiddlewareFuture {
        Box::pin(async move {
            let Some(entry) = self.entries.get(index) else {
                let dispatch = DispatchGuard::begin(ancestors);
                let result = crate::interaction_checkpoint::attempt::with_step_path(
                    path,
                    (self.leaf)(arguments),
                )
                .await;
                if result.is_ok() {
                    dispatch.completed();
                }
                return result;
            };
            let frame = self.frame(index, arguments, path.clone(), ancestors);
            let span = tracing::info_span!("tool_middleware", middleware = %frame.name, chain_version = %self.chain.version,
                tool = %frame.prepared.call.name, step_path = ?path.iter().map(|step| (&step.middleware, &step.key)).collect::<Vec<_>>());
            self.run_frame(frame, entry.clone()).instrument(span).await
        })
    }

    fn frame(
        self: &Arc<Self>,
        index: usize,
        arguments: Value,
        path: Vec<Step>,
        mut ancestors: Vec<Arc<Effects>>,
    ) -> Arc<Frame> {
        let entry = &self.entries[index];
        let mut prepared = self.prepared.clone();
        prepared.arguments = arguments;
        let effects = Arc::new(Effects::default());
        ancestors.push(effects.clone());
        let invocation = self.clone();
        let name = entry.definition.name.clone();
        let next_ancestors = ancestors.clone();
        let identity = resume::FrameIdentity {
            middleware: name.clone(),
            step_path: path.clone(),
        };
        let restored = self.resume.lock().frame(&identity);
        let next = Arc::new(move |arguments, key| {
            let mut path = path.clone();
            path.push(Step {
                middleware: name.clone(),
                key,
            });
            invocation
                .clone()
                .run_at(index + 1, arguments, path, next_ancestors.clone())
        });
        Arc::new(Frame {
            name: entry.definition.name.clone(),
            input: json!({"tool": prepared.call.name, "arguments": prepared.arguments,
                "tool_call_id": prepared.call.id, "session_id": self.chain.session_id,
                "agent": self.chain.agent, "middleware": entry.definition.name}),
            prepared,
            next,
            tools: entry.tools.clone(),
            fetch_hosts: entry.definition.fetch.clone(),
            ancestors,
            effects,
            terminal: self.terminal.clone(),
            state: Mutex::new(frame::FrameState::restored(&restored)),
            identity,
            session: self.chain.session_storage_key.clone(),
            origin: self.origin,
            resume: self.resume.clone(),
            restored,
            attempt: self.attempt.clone(),
        })
    }

    async fn run_frame(
        &self,
        frame: Arc<Frame>,
        entry: Arc<Entry>,
    ) -> Result<ToolMiddlewareOutcome, ToolError> {
        let mut trace = trace::RunTrace::start(&frame.name);
        let execution = script::execute(&frame, &entry.definition).await;
        frame.checkpoint_frame().map_err(ToolError::Recoverable)?;
        if self.terminal.lock().is_some() {
            trace.finish();
            return Err(ToolError::Fatal(anyhow::anyhow!(
                "downstream control flow stopped middleware"
            )));
        }
        let failure = frame
            .state
            .lock()
            .failure
            .clone()
            .or_else(|| execution.err().map(|error| format!("{error:#}")));
        let result = if let Some(failure) = failure {
            trace.failure(&failure);
            frame.on_failure(&entry.definition, failure).await
        } else {
            frame.finish().await
        };
        trace.finish();
        result
    }
}

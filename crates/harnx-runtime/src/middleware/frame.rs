use super::{effects::Effects, envelope::Snapshots};
use anyhow::{anyhow, ensure, Result};
use harnx_core::tool::ToolError;
use harnx_engine::tool::{PreparedToolCall, ToolMiddlewareFuture, ToolMiddlewareOutcome};
use harnx_script::ScriptHostContext;
use parking_lot::Mutex;
use serde_json::Value;
use std::sync::Arc;

/// Logical traversal identity, separate from fresh physical dispatch IDs.
pub(super) type Step = harnx_core::interaction::InteractionStep;

pub(super) type Downstream = dyn Fn(Value, String) -> ToolMiddlewareFuture + Send + Sync;

pub(super) struct Frame {
    pub name: String,
    pub fetch_hosts: Vec<String>,
    pub prepared: PreparedToolCall,
    pub input: Value,
    pub next: Arc<Downstream>,
    pub tools: Vec<harnx_core::tool::ToolDeclaration>,
    pub ancestors: Vec<Arc<Effects>>,
    pub effects: Arc<Effects>,
    pub terminal: Arc<Mutex<Option<Result<ToolMiddlewareOutcome, ToolError>>>>,
    pub state: Mutex<FrameState>,
    pub identity: super::resume::FrameIdentity,
    pub session: String,
    pub origin: harnx_engine::tool::ToolMiddlewareOrigin,
    pub resume: Arc<Mutex<super::checkpoint::ResumeState>>,
    pub restored: super::resume::FrameCheckpoint,
    pub attempt: Option<crate::interaction_checkpoint::attempt::Attempt>,
}

#[derive(Default)]
pub(super) struct FrameState {
    pub snapshots: Snapshots,
    pub response: Option<Value>,
    pub last: Option<Value>,
    pub failure: Option<String>,
    pub stash: std::collections::HashMap<String, Value>,
    pub retained: std::collections::HashMap<String, Vec<String>>,
    pub interaction_ordinal: u64,
    busy: bool,
    ordinal: u64,
}

impl Frame {
    pub fn fail(&self, error: impl std::fmt::Display, host: &ScriptHostContext) -> String {
        let message = error.to_string();
        self.state
            .lock()
            .failure
            .get_or_insert_with(|| message.clone());
        host.control.stop();
        message
    }

    pub fn respond(&self, value: Value) -> Result<Value> {
        let mut state = self.state.lock();
        state.snapshots.validate(&value)?;
        state.response = Some(value);
        Ok(Value::Null)
    }

    pub fn deny(&self, reason: String) -> Result<Value> {
        self.respond(serde_json::json!({
            "output": {"is_error": true, "error": reason, "blocked_by_middleware": self.name},
            "is_error": true,
        }))
    }

    pub fn begin(self: &Arc<Self>) -> Result<OperationGuard> {
        let mut state = self.state.lock();
        ensure!(
            !state.busy,
            "only one outstanding middleware control operation is allowed"
        );
        state.busy = true;
        Ok(OperationGuard(self.clone()))
    }

    fn step_key(&self, id: Option<String>) -> Result<String> {
        let mut state = self.state.lock();
        state.ordinal += 1;
        let key = id.unwrap_or_else(|| state.ordinal.to_string());
        ensure!(!key.is_empty(), "next id must not be empty");
        Ok(key)
    }

    pub async fn call_next(
        self: &Arc<Self>,
        arguments: Option<Value>,
        id: Option<String>,
        host: &ScriptHostContext,
    ) -> Result<Value> {
        let _operation = self.begin()?;
        let key = self.step_key(id)?;
        let arguments = arguments.unwrap_or_else(|| self.prepared.arguments.clone());
        ensure!(
            arguments.is_object(),
            "next arguments must be a JSON object"
        );
        let outcome = (self.next)(arguments, key).await;
        match outcome {
            Ok(ToolMiddlewareOutcome::Completed(result)) => {
                let mut state = self.state.lock();
                let envelope = state.snapshots.insert(result);
                state.last = Some(envelope.clone());
                Ok(envelope)
            }
            outcome => {
                self.terminal.lock().get_or_insert(outcome);
                host.control.stop();
                Err(anyhow!("downstream control flow stopped middleware"))
            }
        }
    }
}

pub(super) struct OperationGuard(Arc<Frame>);

impl Drop for OperationGuard {
    fn drop(&mut self) {
        self.0.state.lock().busy = false;
    }
}

impl FrameState {
    pub fn restored(checkpoint: &super::resume::FrameCheckpoint) -> Self {
        Self {
            stash: checkpoint.stash.clone(),
            retained: checkpoint.retained.clone(),
            ..Default::default()
        }
    }
}

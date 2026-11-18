use super::{envelope, frame::Frame};
use harnx_core::{
    middleware::{MiddlewareOnError, ResolvedMiddleware},
    tool::ToolError,
};
use harnx_engine::tool::ToolMiddlewareOutcome;
use serde_json::Value;

impl Frame {
    pub async fn finish(&self) -> Result<ToolMiddlewareOutcome, ToolError> {
        if self.effects.outcome_unknown() {
            return Ok(self.unknown_outcome());
        }
        let selected = {
            let state = self.state.lock();
            state.response.clone().or_else(|| state.last.clone())
        };
        self.selected_or_dispatch(selected, "default").await
    }

    pub async fn on_failure(
        &self,
        entry: &ResolvedMiddleware,
        failure: String,
    ) -> Result<ToolMiddlewareOutcome, ToolError> {
        if entry.on_error == MiddlewareOnError::Deny {
            return Ok(ToolMiddlewareOutcome::Completed(envelope::denial(
                &self.prepared.call,
                &self.name,
                &format!("middleware '{}': {failure}", self.name),
            )));
        }
        if self.effects.outcome_unknown() {
            return Ok(self.unknown_outcome());
        }
        let last = self.state.lock().last.clone();
        self.selected_or_dispatch(last, "on_error_allow").await
    }

    fn unknown_outcome(&self) -> ToolMiddlewareOutcome {
        ToolMiddlewareOutcome::Completed(envelope::denial(
            &self.prepared.call,
            &self.name,
            "middleware downstream outcome is unknown; no implicit redispatch",
        ))
    }

    async fn selected_or_dispatch(
        &self,
        selected: Option<Value>,
        key: &str,
    ) -> Result<ToolMiddlewareOutcome, ToolError> {
        match selected {
            Some(value) => self
                .state
                .lock()
                .snapshots
                .resolve(&self.prepared.call, value)
                .map(ToolMiddlewareOutcome::Completed)
                .map_err(ToolError::Recoverable),
            None => (self.next)(self.prepared.arguments.clone(), key.into()).await,
        }
    }
}

use super::{frame::Frame, resume::AcceptedInteraction};
use anyhow::{anyhow, ensure, Result};
use harnx_core::interaction::{
    confirm_surface, interaction_id, InteractionIdentity, InteractionRequest, InteractionResponse,
};
use harnx_engine::tool::{ToolCallCheckpoint, ToolMiddlewareOrigin, ToolMiddlewareOutcome};
use harnx_script::ScriptHostContext;
use serde_json::{json, Value};
use std::sync::Arc;

impl Frame {
    pub fn confirm(
        self: &Arc<Self>,
        message: String,
        id: Option<String>,
        host: &ScriptHostContext,
    ) -> Result<Value> {
        let _operation = self.begin()?;
        let key = {
            let mut state = self.state.lock();
            state.interaction_ordinal += 1;
            id.unwrap_or_else(|| state.interaction_ordinal.to_string())
        };
        ensure!(!key.is_empty(), "confirm id must not be empty");
        let request = self.confirm_request(key, message);
        if self.origin == ToolMiddlewareOrigin::Inner {
            self.terminal.lock().get_or_insert_with(|| {
                Ok(ToolMiddlewareOutcome::Completed(super::envelope::denial(
                    &self.prepared.call,
                    &self.name,
                    "interaction required, can't be answered inside a script",
                )))
            });
            host.control.stop();
            return Err(anyhow!(
                "interaction required, can't be answered inside a script"
            ));
        }
        if self.origin == ToolMiddlewareOrigin::Operator {
            self.resume.lock().responses.push(AcceptedInteraction {
                request,
                response: InteractionResponse {
                    action: "approve".into(),
                    data: Default::default(),
                    note: None,
                },
            });
            return Ok(Value::Bool(true));
        }
        let accepted = self
            .resume
            .lock()
            .responses
            .iter()
            .rev()
            .find(|accepted| {
                accepted.request.interaction_id == request.interaction_id
                    && self.request_id(&accepted.request) == request.interaction_id
            })
            .cloned();
        if let Some(accepted) = accepted {
            ensure!(
                matches!(accepted.response.action.as_str(), "approve" | "deny"),
                "invalid accepted confirm action"
            );
            return Ok(Value::Bool(accepted.response.action == "approve"));
        }
        self.terminal
            .lock()
            .get_or_insert(Ok(ToolMiddlewareOutcome::InputRequired {
                interaction: request,
                checkpoint: ToolCallCheckpoint::default(),
            }));
        host.control.stop();
        Err(anyhow!("middleware input required"))
    }

    pub fn answer(&self, key: &str) -> Result<Option<Value>> {
        let resume = self.resume.lock();
        let accepted = resume.responses.iter().rev().find(|accepted| {
            let request = &accepted.request;
            request.producer == format!("middleware:{}", self.name)
                && request.step_path == self.identity.step_path
                && request.key == key
                && request.chain_version == resume.state.chain_version
                && request.interaction_id == self.request_id(request)
        });
        accepted
            .map(|accepted| {
                let mut value = serde_json::to_value(&accepted.response)?;
                value["interaction_id"] = json!(accepted.request.interaction_id);
                Ok(value)
            })
            .transpose()
    }

    fn confirm_request(&self, key: String, message: String) -> InteractionRequest {
        let mut request = InteractionRequest {
            interaction_id: String::new(),
            producer: format!("middleware:{}", self.name),
            step_path: self.identity.step_path.clone(),
            key,
            kind: "confirm".into(),
            payload: confirm_surface(&uuid::Uuid::new_v4().to_string(), &message),
            summary: message,
            chain_version: self.resume.lock().state.chain_version.clone(),
        };
        request.interaction_id = self.request_id(&request);
        request
    }

    fn request_id(&self, request: &InteractionRequest) -> String {
        interaction_id(&InteractionIdentity {
            session: &self.session,
            tool_round_seq: self
                .prepared
                .context
                .evaluation
                .tool_round
                .unwrap_or_default(),
            tool_call_id: self.prepared.call.id.as_deref().unwrap_or_default(),
            chain_version: &request.chain_version,
            producer: &request.producer,
            step_path: &request.step_path,
            key: &request.key,
            kind: &request.kind,
            payload: &request.payload,
        })
    }
}

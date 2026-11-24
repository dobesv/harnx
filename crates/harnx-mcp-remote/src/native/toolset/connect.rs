//! Connect challenges and browser-flow responses.

use super::{text_result, RemoteCredentialsToolset};
use crate::native::registrations::StatusView;
use harnx_credentials::Principal;
use harnx_toolset::{ToolInputRequired, ToolInputResponse, ToolInvocation, ToolInvokeError};
use serde_json::{json, Value};

/// The connect tool's challenge key for one round. Distinct keys per round
/// keep a re-issued challenge from being answered with the previous round's
/// cached response.
fn challenge_key(round: u64) -> String {
    format!("connect-round-{round}")
}

impl RemoteCredentialsToolset {
    fn connect_url(&self, challenge: &str) -> String {
        format!(
            "{}/connect/{challenge}",
            self.base_url.trim_end_matches('/')
        )
    }

    /// Build the `url` challenge for one round of a pending flow.
    pub(super) fn url_challenge(
        &self,
        challenge_id: &str,
        round: u64,
        already_connected_as: Option<&str>,
    ) -> ToolInvokeError {
        let name = self.lifecycle.server_name();
        let mut message = match already_connected_as {
            Some(label) => format!(
                "Connect {name}: open the link to reconnect or switch accounts (currently connected as {label}). The link is single-use and expires in 10 minutes."
            ),
            None => format!(
                "Connect {name}: open the link to sign in with your own account. The link is single-use and expires in 10 minutes."
            ),
        };
        message.push_str(" Return here and click Continue when done.");
        ToolInvokeError::InputRequired(Box::new(ToolInputRequired::url(
            challenge_key(round),
            self.connect_url(challenge_id),
            message,
            json!({ "challenge_id": challenge_id, "round": round }),
        )))
    }

    pub(super) async fn connect_tool(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Value, ToolInvokeError> {
        let principal = self.principal_for(&invocation.context)?;
        let name = self.lifecycle.server_name();

        // Continue/Cancel on an outstanding challenge.
        if let Some(state) = invocation.context.request_state.as_object() {
            let round = state.get("round").and_then(Value::as_u64);
            if let (Some(round), Some(challenge_id)) =
                (round, state.get("challenge_id").and_then(Value::as_str))
            {
                let response = invocation
                    .context
                    .input_responses
                    .get(&challenge_key(round));
                if let Some(response) = response {
                    return self
                        .handle_connect_response(principal, challenge_id, round, response)
                        .await;
                }
            }
        }

        // Fresh call: create a pending flow and hand back the link.
        let already_connected_as = match self.lifecycle.status(&principal).await {
            StatusView::Connected {
                account_label: Some(label),
                expires_at: _,
                needs_login: false,
            } => Some(label),
            _ => None,
        };
        let redirect_uri = format!("{}/callback", self.base_url.trim_end_matches('/'));
        let options = match &self.lifecycle.oauth {
            Some(oauth) => oauth.prepare(redirect_uri).await.map_err(|error| {
                ToolInvokeError::Recoverable(format!("cannot start {name} connect flow: {error}"))
            })?,
            None => harnx_credentials::FlowOptions {
                redirect_uri,
                ..Default::default()
            },
        };
        let flow = self
            .lifecycle
            .flows
            .create(principal.clone(), options)
            .await
            .map_err(|error| {
                ToolInvokeError::Recoverable(format!("cannot start {name} connect flow: {error}"))
            })?;
        Err(self.url_challenge(
            flow.record.challenge_id.expose(),
            1,
            already_connected_as.as_deref(),
        ))
    }

    async fn handle_connect_response(
        &self,
        principal: Principal,
        challenge_id: &str,
        round: u64,
        response: &ToolInputResponse,
    ) -> Result<Value, ToolInvokeError> {
        let name = self.lifecycle.server_name();
        let flow = self
            .lifecycle
            .flows
            .get_by_challenge(challenge_id)
            .await
            .map_err(|error| {
                ToolInvokeError::Recoverable(format!(
                    "connection link expired; call {name}_connect again ({error})"
                ))
            })?;
        let Some(flow) = flow else {
            return Err(ToolInvokeError::Recoverable(format!(
                "connection link expired; call {name}_connect again"
            )));
        };
        if flow.record.owner != principal {
            return Err(ToolInvokeError::Recoverable(
                "this connection link belongs to another session".to_string(),
            ));
        }
        match response.action.as_str() {
            harnx_core::tool::ToolInputRequired::URL_ACTION_CANCEL => {
                let cancelled = self
                    .lifecycle
                    .flows
                    .cancel(challenge_id, flow.revision)
                    .await
                    .map_err(super::recoverable)?;
                match cancelled.record.status {
                    harnx_credentials::FlowStatus::Completed => Ok(text_result(format!(
                        "connection already completed; call {name}_connect again to reconnect"
                    ))),
                    _ => Ok(text_result("connection cancelled".to_string())),
                }
            }
            harnx_core::tool::ToolInputRequired::URL_ACTION_CONTINUE => {
                self.continue_flow(principal, challenge_id, round, flow)
                    .await
            }
            other => Err(ToolInvokeError::Recoverable(format!(
                "unsupported connect response '{other}'; choose continue or cancel"
            ))),
        }
    }
}

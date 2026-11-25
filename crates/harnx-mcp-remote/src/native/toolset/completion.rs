//! Pending-flow continuation and connected-account confirmation.

use super::{text_result, RemoteCredentialsToolset};
use harnx_credentials::{Error as CredentialsError, Principal};
use harnx_toolset::ToolInvokeError;
use serde_json::Value;

impl RemoteCredentialsToolset {
    pub(super) async fn continue_flow(
        &self,
        principal: Principal,
        challenge_id: &str,
        round: u64,
        flow: harnx_credentials::Versioned<harnx_credentials::PendingFlow>,
    ) -> Result<Value, ToolInvokeError> {
        let name = self.lifecycle.server_name();
        use harnx_credentials::flow::FlowOutcome;
        use harnx_credentials::FlowStatus;
        match (&flow.record.status, &flow.record.outcome) {
            (FlowStatus::Pending, _) | (FlowStatus::Claimed, _) => {
                // The flow is still in the browser: re-issue the same link under
                // a new round key so the interaction path asks the user again.
                Err(self.url_challenge(challenge_id, round + 1, None))
            }
            (FlowStatus::Completed, Some(FlowOutcome::Connected { connection_id })) => {
                self.continue_connected_flow(principal, connection_id).await
            }
            (FlowStatus::Completed, None) => Ok(text_result(format!(
                "connection completed; call {name}_connect again to reconnect"
            ))),
            (FlowStatus::Completed, Some(FlowOutcome::Failed { error_message })) => {
                Err(ToolInvokeError::Recoverable(format!(
                    "{}; call {name}_connect again",
                    error_message.expose()
                )))
            }
            (FlowStatus::Failed, outcome) => {
                let message = match outcome {
                    Some(FlowOutcome::Failed { error_message }) => error_message.expose().clone(),
                    _ => "unknown failure".to_string(),
                };
                Err(ToolInvokeError::Recoverable(format!(
                    "{message}; call {name}_connect again"
                )))
            }
            (FlowStatus::Cancelled, _) => Err(ToolInvokeError::Recoverable(format!(
                "connection cancelled; call {name}_connect again"
            ))),
        }
    }

    async fn continue_connected_flow(
        &self,
        principal: Principal,
        connection_id: &str,
    ) -> Result<Value, ToolInvokeError> {
        let name = self.lifecycle.server_name();
        let store = self
            .lifecycle
            .credentials
            .for_principal(principal.clone())
            .map_err(super::recoverable)?;
        let current = store.get().await.map_err(|error| match error {
            CredentialsError::Expired | CredentialsError::NeedsLogin => {
                ToolInvokeError::Recoverable(format!(
                    "{name} credential expired; call {name}_connect"
                ))
            }
            other => super::recoverable(other),
        });
        let Ok(Some(current)) = current else {
            return Ok(text_result(format!(
                "this connection is no longer active; call {name}_connect again"
            )));
        };
        if current.record.connection_id != connection_id {
            return Ok(text_result(format!(
                "a newer connection replaced this one; call {name}_connect again"
            )));
        }
        // A refresh in between kept the connection; count today's tools.
        let catalogs = self
            .lifecycle
            .catalogs
            .for_principal(principal.clone())
            .map_err(super::recoverable)?;
        let count = catalogs
            .get()
            .await
            .ok()
            .flatten()
            .map(|catalog| catalog.record.tools.len())
            .unwrap_or(0);
        let message = match &current.record.account_label {
            Some(label) => {
                format!("Connected as {label}; {count} {name} tools are available from your next message.")
            }
            None => {
                format!("Connected; {count} {name} tools are available from your next message.")
            }
        };
        Ok(text_result(message))
    }
}

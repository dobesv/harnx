//! Global management tools and credential status responses.

use super::RemoteCredentialsToolset;
use crate::native::registrations::{DisconnectOutcome, StatusView};
use harnx_toolset::{ToolInvocation, ToolInvokeError, ToolSpec};
use serde_json::{json, Value};

fn management_spec(name: &str, description: &str, input_required: bool) -> ToolSpec {
    let spec = ToolSpec {
        cancellation_guarantee: Default::default(),
        name: name.to_string(),
        description: description.to_string(),
        input_schema: json!({ "type": "object", "additionalProperties": false }),
        idempotent_hint: true,
        read_only_hint: false,
        // The connect flow suspends for a human; no elapsed-time deadline.
        timeout_secs: Some(0),
        meta: None,
    };
    if input_required {
        spec.with_input_required()
    } else {
        spec
    }
}

/// The global registration's tools: the management set only. Upstream tools
/// arrive through per-principal registrations.
pub(super) fn management_tools() -> Vec<ToolSpec> {
    vec![
        management_spec(
            "connect",
            "Connect this server with your own credentials. Opens a link you complete in a browser; call again to reconnect or switch accounts.",
            true,
        ),
        management_spec(
            "disconnect",
            "Remove your stored credential for this server. Your tools disappear from your sessions.",
            false,
        ),
        management_spec(
            "credential_status",
            "Show whether this server is connected for you, and when the stored credential expires.",
            false,
        ),
    ]
}

impl RemoteCredentialsToolset {
    pub(super) async fn disconnect_tool(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Value, ToolInvokeError> {
        let principal = self.principal_for(&invocation.context)?;
        let outcome = self
            .lifecycle
            .disconnect(&principal)
            .await
            .map_err(|error| ToolInvokeError::Recoverable(format!("{error:#}")))?;
        match outcome {
            DisconnectOutcome::NotConnected => Ok(json!({
                "content": [{ "type": "text", "text": "not connected" }],
            })),
            DisconnectOutcome::Disconnected { account_label } => Ok(json!({
                "content": [{
                    "type": "text",
                    "text": format!(
                        "disconnected{}",
                        account_label
                            .map(|label| format!(" {label}"))
                            .unwrap_or_default()
                    )
                }],
            })),
        }
    }

    pub(super) async fn status_tool(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Value, ToolInvokeError> {
        let principal = self.principal_for(&invocation.context)?;
        match self.lifecycle.status(&principal).await {
            StatusView::Disconnected => Ok(json!({
                "connected": false,
            })),
            StatusView::Connected {
                account_label,
                expires_at,
                needs_login,
            } => Ok(json!({
                "connected": true,
                "account_label": account_label,
                "expires_at": expires_at.to_rfc3339(),
                "status": if needs_login { "needs_login" } else { "active" },
            })),
        }
    }
}

//! Principal-scoped upstream authorization and result normalization.

use super::{transport::call_upstream, RemoteCredentialsToolset};
use harnx_credentials::{CredentialRecord, Error as CredentialsError, Principal, Versioned};
use harnx_toolset::{ToolInvocation, ToolInvokeError};
use serde_json::Value;

impl RemoteCredentialsToolset {
    /// Upstream call (decision 8 + invariant (c)): the caller's own
    /// credential, own catalog, own client.
    pub(super) async fn upstream_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Value, ToolInvokeError> {
        let principal = self.principal_for(&invocation.context)?;
        let credential = self.credential_for(&principal).await?;
        self.authorize_upstream_tool(&principal, &credential.record, &invocation.tool)
            .await?;
        let client = self
            .lifecycle
            .pool
            .client(principal.clone(), &credential)
            .await
            .map_err(|error| ToolInvokeError::Recoverable(format!("{error:#}")))?;
        let result = call_upstream(&invocation, &client).await?;
        // Result limits reuse the bridge's shared implementation (T4).
        let session = invocation
            .context
            .invoking_session
            .as_ref()
            .and_then(|session| {
                harnx_core::cid_url::SessionRef::new(
                    session.agent.clone(),
                    session.session_id.clone(),
                )
                .ok()
            });
        self.lifecycle
            .result_limits
            .normalize(result, session.as_ref())
            .await
    }

    async fn credential_for(
        &self,
        principal: &Principal,
    ) -> Result<Versioned<CredentialRecord>, ToolInvokeError> {
        let name = self.lifecycle.server_name();
        let store = self
            .lifecycle
            .credentials
            .for_principal(principal.clone())
            .map_err(super::recoverable)?;
        let credential = match store.get().await {
            Ok(Some(credential)) => credential,
            Ok(None) => {
                return Err(ToolInvokeError::Recoverable(format!(
                    "{name} is not connected for this session; call {name}_connect"
                )))
            }
            Err(CredentialsError::Expired) => {
                return Err(ToolInvokeError::Recoverable(format!(
                    "{name} credential expired; call {name}_connect"
                )))
            }
            Err(CredentialsError::NeedsLogin) => {
                return Err(ToolInvokeError::Recoverable(format!(
                    "{name} credential needs login; call {name}_connect"
                )))
            }
            Err(error) => return Err(super::recoverable(error)),
        };
        Ok(credential)
    }

    async fn authorize_upstream_tool(
        &self,
        principal: &Principal,
        credential: &CredentialRecord,
        tool_name: &str,
    ) -> Result<(), ToolInvokeError> {
        let name = self.lifecycle.server_name();
        let catalogs = self
            .lifecycle
            .catalogs
            .for_principal(principal.clone())
            .map_err(super::recoverable)?;
        let catalog = catalogs.get().await.map_err(|_| {
            ToolInvokeError::Recoverable(format!(
                "{name} tool list unavailable; call {name}_connect"
            ))
        })?;
        let catalog = match catalog {
            Some(catalog) if catalog.record.connection_id == credential.connection_id => catalog,
            _ => {
                return Err(ToolInvokeError::Recoverable(format!(
                    "{name} tool list unavailable; call {name}_connect"
                )))
            }
        };
        if !catalog
            .record
            .tools
            .iter()
            .any(|tool| tool.name == tool_name)
        {
            return Err(ToolInvokeError::Recoverable(format!(
                "unknown tool '{}' on {name}",
                tool_name
            )));
        }
        Ok(())
    }
}

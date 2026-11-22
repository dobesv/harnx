//! The native `Toolset`: management tools plus per-user upstream dispatch.
//!
//! Management tools (raw names `connect`, `disconnect`, `credential_status`)
//! publish in the server's global registration; upstream tools belong to the
//! per-principal registrations. Every upstream call re-checks the caller's
//! principal against its own credential and catalog (invariant (c)): a forged
//! or mismatched `user_id` never reaches the upstream.

use crate::native::registrations::Lifecycle;
use async_trait::async_trait;
use harnx_credentials::Principal;
use harnx_toolset::{ToolInvocation, ToolInvokeError, ToolSpec, Toolset, MAX_TOOL_INPUT_ROUNDS};
use management::management_tools;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

mod completion;
mod connect;
mod management;
mod transport;
mod upstream;

fn recoverable(error: impl std::fmt::Display) -> ToolInvokeError {
    ToolInvokeError::Recoverable(error.to_string())
}

fn text_result(message: String) -> Value {
    json!({ "content": [{ "type": "text", "text": message }] })
}

pub struct RemoteCredentialsToolset {
    lifecycle: Arc<Lifecycle>,
    /// Base URL the connect links use.
    pub base_url: String,
}

impl RemoteCredentialsToolset {
    pub fn new(lifecycle: Arc<Lifecycle>, base_url: String) -> Self {
        Self {
            lifecycle,
            base_url,
        }
    }

    pub fn lifecycle(&self) -> &Lifecycle {
        &self.lifecycle
    }

    /// The principal this request acts for (decision 7 step 1 and invariant
    /// (c)), or the mode-specific recoverable error.
    fn principal_for(
        &self,
        context: &harnx_toolset::ToolInvocationContext,
    ) -> Result<Principal, ToolInvokeError> {
        let name = self.lifecycle.server_name();
        if self.lifecycle.config.single_user {
            if context.user_id.is_some() {
                return Err(ToolInvokeError::Recoverable(format!(
                    "{name} runs in single-user mode and only serves sessions without a harnx user identity"
                )));
            }
            return Ok(Principal::Ownerless);
        }
        match context.user_id.clone() {
            Some(user_id) => Ok(Principal::User(user_id)),
            None => Err(ToolInvokeError::Recoverable(format!(
                "sign in to harnx (or set user_id / HARNX_USER_ID) to use {name}"
            ))),
        }
    }
}

#[async_trait]
impl Toolset for RemoteCredentialsToolset {
    fn name(&self) -> &str {
        self.lifecycle.server_name()
    }

    fn tools(&self) -> Vec<ToolSpec> {
        management_tools()
    }

    async fn invoke(
        &self,
        tool: &str,
        _args: Value,
        _cancel: CancellationToken,
    ) -> Result<Value, ToolInvokeError> {
        let _ = tool;
        Err(ToolInvokeError::Recoverable(
            "this server requires the invocation context".into(),
        ))
    }

    async fn invoke_with_context(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Value, ToolInvokeError> {
        match invocation.tool.as_str() {
            "connect" => self.connect_tool(invocation).await,
            "disconnect" => self.disconnect_tool(invocation).await,
            "credential_status" => self.status_tool(invocation).await,
            _ => self.upstream_call(invocation).await,
        }
    }
}

const _: () = assert!(MAX_TOOL_INPUT_ROUNDS >= 2);

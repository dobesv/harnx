use crate::lifecycle::SandboxManager;
use crate::mcp::{McpCallError, McpCallErrorKind, McpCaller};
use crate::policy::{EndReason, TerminalError};
use harnx_runtime::nats_session_metadata::{SessionMetadataStore, ToolContextEntry};
use harnx_toolset::{ToolInvocationContext, ToolInvokeError, Toolset};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

mod lifecycle_toolset;
mod proxy;
mod repo_clone;

use lifecycle_toolset::LifecycleToolset;
use proxy::ProxyToolset;

#[cfg(test)]
use lifecycle_toolset::lifecycle_specs;
#[cfg(test)]
use proxy::proxy_spec;

pub const SANDBOX_CONTEXT_KEY: &str = "sandbox";
const MCP_PORT: u16 = 8080;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxBinding {
    pub version: u32,
    pub sandbox_id: String,
}

#[derive(Clone)]
struct Gateway {
    manager: SandboxManager,
    caller: Arc<dyn McpCaller>,
    metadata: SessionMetadataStore,
}

pub fn sandbox_toolsets(
    manager: SandboxManager,
    caller: Arc<dyn McpCaller>,
    metadata: SessionMetadataStore,
) -> Vec<Arc<dyn Toolset>> {
    let gateway = Gateway {
        manager,
        caller,
        metadata,
    };
    vec![
        Arc::new(ProxyToolset::new(
            "bash",
            harnx_bash_tools::builtin_tool_specs(),
            gateway.clone(),
        )),
        Arc::new(ProxyToolset::new(
            "fs",
            harnx_fs_tools::builtin_tool_specs(),
            gateway.clone(),
        )),
        Arc::new(LifecycleToolset { gateway }),
    ]
}

impl Gateway {
    async fn sandbox_id(
        &self,
        explicit: Option<String>,
        context: &ToolInvocationContext,
    ) -> Result<String, ToolInvokeError> {
        if let Some(id) = explicit.filter(|id| !id.trim().is_empty()) {
            return Ok(id);
        }
        let session_id = context
            .invoking_session_id
            .as_deref()
            .ok_or_else(missing_binding)?;
        let tool_context = self
            .metadata
            .get_tool_context(session_id)
            .await
            .map_err(recoverable)?
            .ok_or_else(missing_binding)?;
        let value = tool_context
            .values
            .get(SANDBOX_CONTEXT_KEY)
            .cloned()
            .ok_or_else(missing_binding)?;
        let binding: SandboxBinding = serde_json::from_value(value).map_err(|error| {
            ToolInvokeError::Recoverable(format!("invalid ambient sandbox binding: {error}"))
        })?;
        if binding.version != 1 || binding.sandbox_id.trim().is_empty() {
            return Err(ToolInvokeError::Recoverable(
                "invalid ambient sandbox binding; call sandbox_connect again".to_string(),
            ));
        }
        Ok(binding.sandbox_id)
    }

    async fn bind(&self, session_id: &str, sandbox_id: &str) -> Result<(), ToolInvokeError> {
        self.metadata
            .replace_tool_context_value(
                ToolContextEntry {
                    session_id,
                    key: SANDBOX_CONTEXT_KEY,
                },
                serde_json::to_value(SandboxBinding {
                    version: 1,
                    sandbox_id: sandbox_id.to_string(),
                })
                .map_err(fatal)?,
            )
            .await
            .map_err(recoverable)?;
        Ok(())
    }

    async fn ambient_binding(
        &self,
        session_id: Option<&str>,
    ) -> Result<Option<String>, ToolInvokeError> {
        let Some(session_id) = session_id else {
            return Ok(None);
        };
        let Some(context) = self
            .metadata
            .get_tool_context(session_id)
            .await
            .map_err(recoverable)?
        else {
            return Ok(None);
        };
        context
            .values
            .get(SANDBOX_CONTEXT_KEY)
            .cloned()
            .map(serde_json::from_value::<SandboxBinding>)
            .transpose()
            .map(|binding| binding.map(|binding| binding.sandbox_id))
            .map_err(|error| {
                ToolInvokeError::Recoverable(format!("invalid ambient sandbox binding: {error}"))
            })
    }

    async fn call_remote(
        &self,
        sandbox_id: &str,
        endpoint: &str,
        tool: &str,
        args: Map<String, Value>,
        capabilities: BTreeSet<String>,
        cancel: CancellationToken,
    ) -> Result<Value, ToolInvokeError> {
        self.call_with_activity_heartbeat(sandbox_id, endpoint, tool, args, capabilities, cancel)
            .await
            .map_err(remote_error)
    }

    async fn call_with_activity_heartbeat(
        &self,
        sandbox_id: &str,
        endpoint: &str,
        tool: &str,
        args: Map<String, Value>,
        capabilities: BTreeSet<String>,
        cancel: CancellationToken,
    ) -> Result<Value, McpCallError> {
        let call = self.caller.call(
            sandbox_id,
            endpoint,
            tool,
            args,
            capabilities,
            cancel.clone(),
        );
        tokio::pin!(call);
        let heartbeat_cancel = cancel.child_token();
        let heartbeat = async {
            let first = tokio::time::Instant::now() + Duration::from_secs(60);
            let mut ticker = tokio::time::interval_at(first, Duration::from_secs(60));
            loop {
                ticker.tick().await;
                if let Err(error) = self
                    .manager
                    .record_activity(sandbox_id, &heartbeat_cancel)
                    .await
                {
                    if heartbeat_cancel.is_cancelled() {
                        // MCP caller owns stop acknowledgement. Finishing this
                        // auxiliary would drop its waiter before that settles.
                        std::future::pending::<()>().await;
                    }
                    log::warn!("failed to refresh activity for sandbox '{sandbox_id}': {error:#}");
                }
            }
        };
        tokio::pin!(heartbeat);
        tokio::select! {
            result = &mut call => result,
            () = &mut heartbeat => unreachable!("activity heartbeat never completes"),
        }
    }
}
fn mcp_endpoint(ip: &str) -> String {
    if ip.contains(':') {
        format!("http://[{ip}]:{MCP_PORT}/mcp")
    } else {
        format!("http://{ip}:{MCP_PORT}/mcp")
    }
}

fn missing_binding() -> ToolInvokeError {
    ToolInvokeError::Recoverable(
        "no sandbox is bound to this session; call sandbox_connect or provide sandbox_id"
            .to_string(),
    )
}

fn recoverable(error: impl std::fmt::Display) -> ToolInvokeError {
    ToolInvokeError::Recoverable(error.to_string())
}

fn fatal(error: impl std::fmt::Display) -> ToolInvokeError {
    ToolInvokeError::Fatal(error.to_string())
}
fn lifecycle_error(error: anyhow::Error) -> ToolInvokeError {
    let cancelled = error.chain().any(|source| {
        source
            .downcast_ref::<TerminalError>()
            .is_some_and(|error| error.end_reason == EndReason::Cancelled)
    });
    if cancelled {
        ToolInvokeError::Fatal(error.to_string())
    } else {
        ToolInvokeError::Recoverable(error.to_string())
    }
}

fn remote_error(error: McpCallError) -> ToolInvokeError {
    match error.kind {
        McpCallErrorKind::Cancelled
        | McpCallErrorKind::Serialization
        | McpCallErrorKind::TransportClosed => ToolInvokeError::Fatal(error.to_string()),
        McpCallErrorKind::Connect | McpCallErrorKind::Call | McpCallErrorKind::DeadlineExceeded => {
            ToolInvokeError::Recoverable(error.to_string())
        }
    }
}

#[cfg(test)]
#[path = "toolsets_tests.rs"]
mod tests;

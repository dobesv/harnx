use crate::lifecycle::SandboxManager;
use crate::mcp::{McpCallError, McpCallErrorKind, McpCaller};
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
        let mut last_error = None;
        for attempt in 1..=3 {
            let outcome = self
                .call_with_activity_heartbeat(
                    sandbox_id,
                    endpoint,
                    tool,
                    args.clone(),
                    capabilities.clone(),
                    cancel.clone(),
                )
                .await;
            match outcome {
                Ok(result) => return Ok(result),
                Err(error) if error.kind == McpCallErrorKind::Cancelled => {
                    return Err(ToolInvokeError::Fatal("tool call cancelled".to_string()));
                }
                Err(error) if error.kind == McpCallErrorKind::Connect && attempt < 3 => {
                    last_error = Some(error);
                    retry_delay(&cancel).await?;
                }
                Err(error) => return Err(remote_error(error)),
            }
        }
        Err(remote_error(last_error.expect("retry records its error")))
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
        let call = self
            .caller
            .call(sandbox_id, endpoint, tool, args, capabilities, cancel);
        tokio::pin!(call);
        let first_heartbeat = tokio::time::Instant::now() + Duration::from_secs(60);
        let mut heartbeat = tokio::time::interval_at(first_heartbeat, Duration::from_secs(60));
        loop {
            tokio::select! {
                result = &mut call => return result,
                _ = heartbeat.tick() => {
                    if let Err(error) = self.manager.record_activity(sandbox_id).await {
                        log::warn!("failed to refresh activity for sandbox '{sandbox_id}': {error:#}");
                    }
                }
            }
        }
    }
}

async fn retry_delay(cancel: &CancellationToken) -> Result<(), ToolInvokeError> {
    tokio::select! {
        _ = cancel.cancelled() => Err(ToolInvokeError::Fatal("tool call cancelled".to_string())),
        () = tokio::time::sleep(Duration::from_secs(2)) => Ok(()),
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

fn remote_error(error: McpCallError) -> ToolInvokeError {
    ToolInvokeError::Recoverable(error.to_string())
}

#[cfg(test)]
#[path = "toolsets_tests.rs"]
mod tests;

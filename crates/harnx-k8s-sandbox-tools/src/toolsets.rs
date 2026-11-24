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

mod attachment_write;
mod binding;
mod connection;
mod lifecycle_toolset;
mod proxy;
mod repo_clone;
mod specs;

use lifecycle_toolset::LifecycleToolset;
use proxy::ProxyToolset;

#[cfg(test)]
use proxy::proxy_spec;
#[cfg(test)]
use specs::lifecycle_specs;

/// MCP ports for bash and fs services in a sandbox.
#[derive(Clone, Copy, Debug)]
pub struct SandboxPorts {
    pub bash: u16,
    pub fs: u16,
}

impl Default for SandboxPorts {
    fn default() -> Self {
        Self {
            bash: 3002,
            fs: 3003,
        }
    }
}

pub const SANDBOX_CONTEXT_KEY: &str = "sandbox";

/// Registered names for the gateway's three native toolsets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxToolsetNames {
    /// Registered name for the bash proxy toolset.
    pub bash: String,
    /// Registered name for the filesystem proxy toolset.
    pub fs: String,
    /// Registered name for the sandbox lifecycle toolset.
    pub sandbox: String,
}

impl Default for SandboxToolsetNames {
    fn default() -> Self {
        Self {
            bash: "bash".to_string(),
            fs: "fs".to_string(),
            sandbox: "sandbox".to_string(),
        }
    }
}

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
    bash_mcp_port: u16,
    sandbox_toolset_name: String,
}

pub fn sandbox_toolsets(
    manager: SandboxManager,
    caller: Arc<dyn McpCaller>,
    metadata: SessionMetadataStore,
    ports: SandboxPorts,
) -> Vec<Arc<dyn Toolset>> {
    sandbox_toolsets_with_names(
        manager,
        caller,
        metadata,
        ports,
        SandboxToolsetNames::default(),
    )
}

/// Build the bash, filesystem, and lifecycle toolsets with custom registration names.
pub fn sandbox_toolsets_with_names(
    manager: SandboxManager,
    caller: Arc<dyn McpCaller>,
    metadata: SessionMetadataStore,
    ports: SandboxPorts,
    names: SandboxToolsetNames,
) -> Vec<Arc<dyn Toolset>> {
    let SandboxToolsetNames { bash, fs, sandbox } = names;
    let gateway = Gateway {
        manager,
        caller,
        metadata,
        bash_mcp_port: ports.bash,
        sandbox_toolset_name: sandbox.clone(),
    };
    vec![
        Arc::new(ProxyToolset::new(
            bash,
            harnx_bash_tools::builtin_tool_specs(),
            gateway.clone(),
            proxy::Backend::Bash(ports.bash),
        )),
        Arc::new(ProxyToolset::new(
            fs,
            harnx_fs_tools::builtin_tool_specs(),
            gateway.clone(),
            proxy::Backend::Fs(ports.fs),
        )),
        Arc::new(LifecycleToolset {
            name: sandbox,
            gateway,
        }),
    ]
}

/// Decode a stored raw binding value into its usable sandbox ID.
///
/// Well-formed but unusable bindings (unknown version or blank ID) yield
/// `None`; a malformed value errors. Shared by every ambient-binding read
/// so the validity rules live in one place.
fn usable_binding_id(value: Option<&Value>) -> Result<Option<String>, ToolInvokeError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let binding: SandboxBinding = serde_json::from_value(value.clone()).map_err(|error| {
        ToolInvokeError::Recoverable(format!("invalid ambient sandbox binding: {error}"))
    })?;
    Ok(
        (binding.version == 1 && !binding.sandbox_id.trim().is_empty())
            .then_some(binding.sandbox_id),
    )
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
            .ok_or_else(|| missing_binding(&self.sandbox_toolset_name))?;
        let tool_context = self
            .metadata
            .get_tool_context(session_id)
            .await
            .map_err(recoverable)?
            .ok_or_else(|| missing_binding(&self.sandbox_toolset_name))?;
        let value = tool_context
            .values
            .get(SANDBOX_CONTEXT_KEY)
            .cloned()
            .ok_or_else(|| missing_binding(&self.sandbox_toolset_name))?;
        let binding: SandboxBinding = serde_json::from_value(value).map_err(|error| {
            ToolInvokeError::Recoverable(format!("invalid ambient sandbox binding: {error}"))
        })?;
        if binding.version != 1 || binding.sandbox_id.trim().is_empty() {
            return Err(ToolInvokeError::Recoverable(format!(
                "invalid ambient sandbox binding; call {}_connect again",
                self.sandbox_toolset_name
            )));
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

    /// Bind `sandbox_id` to the session only while the session's stored
    /// binding still matches `observed`, the raw binding value the caller
    /// read when it started, returning whether the binding was applied.
    ///
    /// A connect that resolves or activates its sandbox slowly must not
    /// overwrite a binding written by a concurrent or later connect, so the
    /// delayed unready bind goes through here. The precondition and the
    /// write are one compare-and-set on the metadata revision; `Ok(false)`
    /// means the precondition no longer held and the bind was skipped —
    /// whether the claim is then destroyed depends on the binding re-read
    /// in the destroy path, not on this result alone.
    async fn bind_if_unchanged(
        &self,
        session_id: &str,
        sandbox_id: &str,
        observed: Option<&Value>,
    ) -> Result<bool, ToolInvokeError> {
        let value = serde_json::to_value(SandboxBinding {
            version: 1,
            sandbox_id: sandbox_id.to_string(),
        })
        .map_err(fatal)?;
        self.metadata
            .replace_tool_context_value_if(
                ToolContextEntry {
                    session_id,
                    key: SANDBOX_CONTEXT_KEY,
                },
                observed,
                value,
            )
            .await
            .map_err(recoverable)
    }

    /// The session's raw stored binding value, present-but-unusable values
    /// included.
    ///
    /// Callers that will conditionally overwrite the binding compare
    /// against this raw value, so a stored binding that is present but
    /// unusable is treated as present, not as absent.
    async fn raw_binding(
        &self,
        session_id: Option<&str>,
    ) -> Result<Option<Value>, ToolInvokeError> {
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
        Ok(context.values.get(SANDBOX_CONTEXT_KEY).cloned())
    }

    /// The session's usable ambient binding, if any.
    ///
    /// Unlike [`Gateway::sandbox_id`], well-formed but unusable bindings
    /// (unknown version or blank ID) yield `None` instead of an error, so
    /// callers like `sandbox_connect` can fall back to creating a fresh
    /// sandbox and replacing the binding. Malformed metadata still errors.
    async fn ambient_binding(
        &self,
        session_id: Option<&str>,
    ) -> Result<Option<String>, ToolInvokeError> {
        usable_binding_id(self.raw_binding(session_id).await?.as_ref())
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
fn mcp_endpoint(ip: &str, port: u16) -> String {
    if ip.contains(':') {
        format!("http://[{ip}]:{port}/mcp")
    } else {
        format!("http://{ip}:{port}/mcp")
    }
}

fn missing_binding(sandbox_toolset_name: &str) -> ToolInvokeError {
    ToolInvokeError::Recoverable(format!(
        "no sandbox is bound to this session; call {sandbox_toolset_name}_connect or provide sandbox_id"
    ))
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

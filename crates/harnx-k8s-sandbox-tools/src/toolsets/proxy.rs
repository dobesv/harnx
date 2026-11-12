use super::{attachment_write, lifecycle_error, mcp_endpoint, Gateway};
use async_trait::async_trait;
use harnx_toolset::{ToolInvocation, ToolInvocationContext, ToolInvokeError, ToolSpec, Toolset};
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
use tokio_util::sync::CancellationToken;

pub(super) struct ProxyToolset {
    name: String,
    specs: Vec<ToolSpec>,
    gateway: Gateway,
    backend: Backend,
}

pub(super) enum Backend {
    Bash(u16),
    Fs(u16),
}

impl Backend {
    fn port(&self) -> u16 {
        match self {
            Self::Bash(port) | Self::Fs(port) => *port,
        }
    }

    fn translates_attachment(&self, tool: &str) -> bool {
        matches!(self, Self::Fs(_)) && tool == "write_attachment"
    }
}

struct ResolvedInvocation {
    sandbox_id: String,
    endpoint: String,
    tool: String,
    args: Map<String, Value>,
    capabilities: BTreeSet<String>,
    cancel: CancellationToken,
}

impl ProxyToolset {
    pub(super) fn new(
        name: String,
        specs: Vec<ToolSpec>,
        gateway: Gateway,
        backend: Backend,
    ) -> Self {
        Self {
            name,
            specs: specs.into_iter().map(proxy_spec).collect(),
            gateway,
            backend,
        }
    }

    async fn resolve(
        &self,
        invocation: ToolInvocation,
    ) -> Result<ResolvedInvocation, ToolInvokeError> {
        let ToolInvocation {
            tool,
            args,
            context,
            cancel,
        } = invocation;
        if !self.specs.iter().any(|spec| spec.name == tool) {
            return Err(ToolInvokeError::Recoverable(format!(
                "unknown {} tool: {tool}",
                self.name
            )));
        }
        let Value::Object(mut args) = args else {
            return Err(ToolInvokeError::Recoverable(
                "tool arguments must be an object".to_string(),
            ));
        };
        let explicit = sandbox_override(args.remove("sandbox_id"))?;
        let sandbox_id = self.gateway.sandbox_id(explicit, &context).await?;
        let (tool, args) = if self.backend.translates_attachment(&tool) {
            let args = attachment_write::resolve(args, self.gateway.metadata.jetstream()).await?;
            ("write_bytes".to_string(), args)
        } else {
            (tool, args)
        };
        let pod_ip = self
            .gateway
            .manager
            .ensure_active(&sandbox_id, &cancel)
            .await
            .map_err(lifecycle_error)?;
        Ok(ResolvedInvocation {
            sandbox_id,
            endpoint: mcp_endpoint(&pod_ip, self.backend.port()),
            tool,
            args,
            capabilities: context.capabilities,
            cancel,
        })
    }

    async fn forward(&self, invocation: ResolvedInvocation) -> Result<Value, ToolInvokeError> {
        let result = self
            .gateway
            .call_with_activity_heartbeat(
                &invocation.sandbox_id,
                &invocation.endpoint,
                &invocation.tool,
                invocation.args,
                invocation.capabilities,
                invocation.cancel,
            )
            .await;
        match (&self.backend, invocation.tool.as_str()) {
            (Backend::Fs(_), "write_bytes") => result.map_err(attachment_write::remote_error),
            _ => result.map_err(super::remote_error),
        }
    }

    async fn execute(&self, invocation: ToolInvocation) -> Result<Value, ToolInvokeError> {
        let cancel = invocation.cancel.clone();
        if cancel.is_cancelled() {
            return Err(ToolInvokeError::Fatal("tool call cancelled".into()));
        }
        // Resolving may activate a sandbox. Keep ownership until that work
        // settles; the remote caller checks cancellation before sending a call.
        let resolved = self.resolve(invocation).await?;
        // Once forwarding starts, the MCP caller owns cancellation so it can
        // notify the in-sandbox server before completing the invocation.
        self.forward(resolved).await
    }
}

#[async_trait]
impl Toolset for ProxyToolset {
    fn name(&self) -> &str {
        &self.name
    }

    fn tools(&self) -> Vec<ToolSpec> {
        self.specs.clone()
    }

    async fn invoke(
        &self,
        tool: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<Value, ToolInvokeError> {
        self.execute(ToolInvocation {
            tool: tool.to_string(),
            args,
            context: ToolInvocationContext::default(),
            cancel,
        })
        .await
    }

    async fn invoke_with_context(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Value, ToolInvokeError> {
        self.execute(invocation).await
    }
}

fn sandbox_override(value: Option<Value>) -> Result<Option<String>, ToolInvokeError> {
    match value {
        Some(Value::String(id)) => Ok(Some(id)),
        Some(_) => Err(ToolInvokeError::Recoverable(
            "sandbox_id must be a string".to_string(),
        )),
        None => Ok(None),
    }
}

pub(super) fn proxy_spec(mut spec: ToolSpec) -> ToolSpec {
    if !spec.input_schema.is_object() {
        spec.input_schema = json!({"type": "object"});
    }
    let schema = spec
        .input_schema
        .as_object_mut()
        .expect("schema was normalized to an object");
    if !schema.get("properties").is_some_and(Value::is_object) {
        schema.insert("properties".to_string(), json!({}));
    }
    let properties = schema["properties"]
        .as_object_mut()
        .expect("properties were normalized to an object");
    properties.insert(
        "sandbox_id".to_string(),
        json!({
            "type": "string",
            "description": "Optional sandbox override. When omitted, use the sandbox bound to the current Harnx session."
        }),
    );
    spec.timeout_secs = Some(0);
    // The sandbox's tools are reached over MCP, which has no way to deliver a
    // new-message signal, so a proxied wait cannot stop on one.
    if let Some(meta) = spec.meta.as_mut() {
        meta.remove(harnx_toolset::STOP_ON_NEW_MESSAGE_META_KEY);
    }
    spec
}

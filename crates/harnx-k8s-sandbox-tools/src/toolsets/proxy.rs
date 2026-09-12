use super::{lifecycle_error, mcp_endpoint, Gateway};
use async_trait::async_trait;
use harnx_toolset::{ToolInvocation, ToolInvocationContext, ToolInvokeError, ToolSpec, Toolset};
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
use tokio_util::sync::CancellationToken;

pub(super) struct ProxyToolset {
    name: &'static str,
    specs: Vec<ToolSpec>,
    gateway: Gateway,
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
    pub(super) fn new(name: &'static str, specs: Vec<ToolSpec>, gateway: Gateway) -> Self {
        Self {
            name,
            specs: specs.into_iter().map(proxy_spec).collect(),
            gateway,
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
        let mut args = args.as_object().cloned().ok_or_else(|| {
            ToolInvokeError::Recoverable("tool arguments must be an object".to_string())
        })?;
        let explicit = sandbox_override(args.remove("sandbox_id"))?;
        let sandbox_id = self.gateway.sandbox_id(explicit, &context).await?;
        let pod_ip = self
            .gateway
            .manager
            .ensure_active(&sandbox_id, &cancel)
            .await
            .map_err(lifecycle_error)?;
        Ok(ResolvedInvocation {
            sandbox_id,
            endpoint: mcp_endpoint(&pod_ip),
            tool: format!("{}_{}", self.name, tool),
            args,
            capabilities: context.capabilities,
            cancel,
        })
    }

    async fn forward(&self, invocation: ResolvedInvocation) -> Result<Value, ToolInvokeError> {
        self.gateway
            .call_remote(
                &invocation.sandbox_id,
                &invocation.endpoint,
                &invocation.tool,
                invocation.args,
                invocation.capabilities,
                invocation.cancel,
            )
            .await
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
        self.name
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
    spec
}

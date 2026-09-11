use super::repo_clone::{clone_repo, CloneRequest, RepoSpec};
use super::{fatal, mcp_endpoint, recoverable, Gateway, SANDBOX_CONTEXT_KEY};
use async_trait::async_trait;
use harnx_runtime::nats_session_metadata::ToolContextEntry;
use harnx_toolset::{ToolInvocation, ToolInvocationContext, ToolInvokeError, ToolSpec, Toolset};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub(super) struct LifecycleToolset {
    pub(super) gateway: Gateway,
}

#[derive(Deserialize)]
struct ConnectArgs {
    #[serde(default)]
    sandbox_id: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    repos: Vec<RepoSpec>,
}

#[derive(Deserialize)]
struct StatusArgs {
    #[serde(default)]
    sandbox_id: Option<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[derive(Deserialize)]
struct ReleaseArgs {
    #[serde(default)]
    sandbox_id: Option<String>,
    #[serde(default)]
    destroy: bool,
}

impl LifecycleToolset {
    async fn connect(
        &self,
        args: Value,
        context: &ToolInvocationContext,
        cancel: CancellationToken,
    ) -> Result<Value, ToolInvokeError> {
        let args: ConnectArgs = parse_args("sandbox_connect", args)?;
        let session_id = context.invoking_session_id.as_deref().ok_or_else(|| {
            ToolInvokeError::Recoverable(
                "sandbox_connect requires an invoking Harnx session".to_string(),
            )
        })?;
        let sandbox_id = self.resolve_connection(&args, context).await?;
        let pod_ip = self
            .gateway
            .manager
            .ensure_active(&sandbox_id)
            .await
            .map_err(recoverable)?;
        let endpoint = mcp_endpoint(&pod_ip);
        self.bind_ready_sandbox(session_id, &sandbox_id).await?;

        let mut repos = Vec::with_capacity(args.repos.len());
        for repo in args.repos {
            repos.push(
                clone_repo(
                    &self.gateway,
                    CloneRequest {
                        sandbox_id: &sandbox_id,
                        endpoint: &endpoint,
                        repo,
                        cancel: cancel.clone(),
                    },
                )
                .await?,
            );
        }
        Ok(json!({"sandbox_id": sandbox_id, "repos": repos}))
    }

    async fn resolve_connection(
        &self,
        args: &ConnectArgs,
        context: &ToolInvocationContext,
    ) -> Result<String, ToolInvokeError> {
        if let Some(id) = args.sandbox_id.as_ref().filter(|id| !id.trim().is_empty()) {
            return Ok(id.clone());
        }
        self.gateway
            .manager
            .create(&context.call_id, args.description.as_deref())
            .await
            .map_err(recoverable)
    }

    async fn bind_ready_sandbox(
        &self,
        session_id: &str,
        sandbox_id: &str,
    ) -> Result<(), ToolInvokeError> {
        self.gateway
            .bind(session_id, sandbox_id)
            .await
            .map_err(|error| {
                ToolInvokeError::Recoverable(format!(
                    "sandbox '{sandbox_id}' is ready but its session binding failed: {error}"
                ))
            })
    }

    async fn status(
        &self,
        args: Value,
        context: &ToolInvocationContext,
    ) -> Result<Value, ToolInvokeError> {
        let args: StatusArgs = parse_args("sandbox_status", args)?;
        let id = self.gateway.sandbox_id(args.sandbox_id, context).await?;
        let status = self
            .gateway
            .manager
            .status(&id, args.timeout_secs.map(Duration::from_secs))
            .await
            .map_err(recoverable)?;
        serde_json::to_value(status).map_err(fatal)
    }

    async fn release(
        &self,
        args: Value,
        context: &ToolInvocationContext,
    ) -> Result<Value, ToolInvokeError> {
        let args: ReleaseArgs = parse_args("sandbox_release", args)?;
        let ambient = self
            .gateway
            .ambient_binding(context.invoking_session_id.as_deref())
            .await?;
        let id = self.gateway.sandbox_id(args.sandbox_id, context).await?;
        self.gateway.caller.disconnect(&id).await;
        let status = self
            .gateway
            .manager
            .release(&id, args.destroy)
            .await
            .map_err(recoverable)?;
        if args.destroy && ambient.as_deref() == Some(id.as_str()) {
            self.clear_binding(context, &id).await?;
        }
        Ok(release_result(status, args.destroy))
    }

    async fn clear_binding(
        &self,
        context: &ToolInvocationContext,
        sandbox_id: &str,
    ) -> Result<(), ToolInvokeError> {
        let Some(session_id) = context.invoking_session_id.as_deref() else {
            return Ok(());
        };
        self.gateway
            .metadata
            .remove_tool_context_value(ToolContextEntry {
                session_id,
                key: SANDBOX_CONTEXT_KEY,
            })
            .await
            .map_err(recoverable)?;
        log::debug!("cleared sandbox '{sandbox_id}' binding from session '{session_id}'");
        Ok(())
    }

    async fn invoke_inner(&self, invocation: ToolInvocation) -> Result<Value, ToolInvokeError> {
        if invocation.cancel.is_cancelled() {
            return Err(ToolInvokeError::Fatal("tool call cancelled".into()));
        }
        let ToolInvocation {
            tool,
            args,
            context,
            cancel,
        } = invocation;
        match tool.as_str() {
            "connect" => self.connect(args, &context, cancel).await,
            "status" => self.status(args, &context).await,
            "release" => self.release(args, &context).await,
            _ => Err(ToolInvokeError::Recoverable(format!(
                "unknown sandbox tool: {tool}"
            ))),
        }
    }
}

#[async_trait]
impl Toolset for LifecycleToolset {
    fn name(&self) -> &str {
        "sandbox"
    }

    fn tools(&self) -> Vec<ToolSpec> {
        lifecycle_specs()
    }

    async fn invoke(
        &self,
        tool: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<Value, ToolInvokeError> {
        self.invoke_inner(ToolInvocation {
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
        // Retain in-flight lifecycle operations and remote clone calls until
        // their owner returns; dropping this future is not a cleanup guarantee.
        self.invoke_inner(invocation).await
    }
}

fn release_result(status: &str, destroy: bool) -> Value {
    json!({
        "status": status,
        "message": if destroy {
            "Sandbox and storage permanently deleted."
        } else {
            "Sandbox suspended. Storage preserved. It will wake on the next tool call."
        }
    })
}

fn parse_args<T: for<'de> Deserialize<'de>>(tool: &str, args: Value) -> Result<T, ToolInvokeError> {
    serde_json::from_value(args)
        .map_err(|error| ToolInvokeError::Recoverable(format!("invalid {tool} arguments: {error}")))
}

pub(super) fn lifecycle_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            cancellation_guarantee: Default::default(),
            name: "connect".to_string(),
            description: "Connect this Harnx session to an existing Kubernetes sandbox or create and start a new sandbox.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "sandbox_id": {"type": "string", "description": "Existing sandbox ID; omit to create one."},
                    "description": {"type": "string"},
                    "repos": {"type": "array", "items": {"type": "object", "properties": {
                        "repo_url": {"type": "string"}, "branch": {"type": "string"}, "path": {"type": "string"}
                    }, "required": ["repo_url"]}}
                }
            }),
            idempotent_hint: false,
            read_only_hint: false,
            timeout_secs: Some(0),
            meta: None,
        },
        ToolSpec {
            cancellation_guarantee: Default::default(),
            name: "status".to_string(),
            description: "Observe sandbox status without waking it or extending its activity.".to_string(),
            input_schema: json!({"type": "object", "properties": {
                "sandbox_id": {"type": "string"},
                "timeout_secs": {"type": "integer", "minimum": 0}
            }}),
            idempotent_hint: true,
            read_only_hint: true,
            timeout_secs: Some(0),
            meta: None,
        },
        ToolSpec {
            cancellation_guarantee: Default::default(),
            name: "release".to_string(),
            description: "Hibernate the current sandbox while preserving storage, or permanently destroy it.".to_string(),
            input_schema: json!({"type": "object", "properties": {
                "sandbox_id": {"type": "string"},
                "destroy": {"type": "boolean", "default": false}
            }}),
            idempotent_hint: false,
            read_only_hint: false,
            timeout_secs: Some(0),
            meta: None,
        },
    ]
}

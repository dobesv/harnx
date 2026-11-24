use super::repo_clone::{clone_repo, CloneRequest, RepoSpec};
use super::specs::lifecycle_specs;
use super::specs::parse_args;
use super::{fatal, lifecycle_error, mcp_endpoint, Gateway};
use async_trait::async_trait;
use harnx_toolset::{ToolInvocation, ToolInvocationContext, ToolInvokeError, ToolSpec, Toolset};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub(super) struct LifecycleToolset {
    pub(super) name: String,
    pub(super) gateway: Gateway,
}

#[derive(Deserialize)]
pub(super) struct ConnectArgs {
    #[serde(default)]
    pub(super) sandbox_id: Option<String>,
    #[serde(default)]
    pub(super) description: Option<String>,
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
        let connection = self.resolve_connection(&args, context, &cancel).await?;
        let (sandbox_id, pod_ip) = self
            .activate_sandbox(session_id, &connection, &cancel)
            .await?;
        let endpoint = mcp_endpoint(&pod_ip, self.gateway.bash_mcp_port);
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

    async fn status(
        &self,
        args: Value,
        context: &ToolInvocationContext,
        cancel: &CancellationToken,
    ) -> Result<Value, ToolInvokeError> {
        let args: StatusArgs = parse_args("sandbox_status", args)?;
        let id = self.gateway.sandbox_id(args.sandbox_id, context).await?;
        let status = self
            .gateway
            .manager
            .status(&id, args.timeout_secs.map(Duration::from_secs), cancel)
            .await
            .map_err(lifecycle_error)?;
        serde_json::to_value(status).map_err(fatal)
    }

    async fn release(
        &self,
        args: Value,
        context: &ToolInvocationContext,
        cancel: &CancellationToken,
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
            .release(&id, args.destroy, cancel)
            .await
            .map_err(lifecycle_error)?;
        if args.destroy && ambient.as_deref() == Some(id.as_str()) {
            self.clear_binding(context, &id).await?;
        }
        Ok(release_result(status, args.destroy))
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
            "status" => self.status(args, &context, &cancel).await,
            "release" => self.release(args, &context, &cancel).await,
            _ => Err(ToolInvokeError::Recoverable(format!(
                "unknown sandbox tool: {tool}"
            ))),
        }
    }
}

#[async_trait]
impl Toolset for LifecycleToolset {
    fn name(&self) -> &str {
        &self.name
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

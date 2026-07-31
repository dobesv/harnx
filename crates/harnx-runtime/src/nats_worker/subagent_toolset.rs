//! NATS toolset for nested sub-agent turns.

use crate::nats_client_session::{ThinClientConfig, ThinClientSession};
use async_nats::jetstream;
use async_trait::async_trait;
use harnx_core::event::NullSink;
use harnx_core::package_namespace::sanitize_for_tool_name;
use harnx_toolset::{ToolInvokeError, ToolSpec, Toolset};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Walking-skeleton toolset that exposes one blocking prompt tool.
pub(crate) struct SubagentPromptToolset {
    agent: String,
    cluster: String,
    server_name: String,
    tool_name: String,
    client: async_nats::Client,
    jetstream: jetstream::Context,
}

impl SubagentPromptToolset {
    pub(crate) fn new(
        agent: impl Into<String>,
        cluster: impl Into<String>,
        client: async_nats::Client,
        jetstream: jetstream::Context,
    ) -> Self {
        let agent = agent.into();
        let server_name = sanitize_for_tool_name(&agent);
        let tool_name = format!("{server_name}_session_prompt");
        Self {
            agent,
            cluster: cluster.into(),
            server_name,
            tool_name,
            client,
            jetstream,
        }
    }
}

#[derive(Deserialize)]
struct PromptArgs {
    message: String,
    #[serde(default)]
    session_id: Option<String>,
}

#[async_trait]
impl Toolset for SubagentPromptToolset {
    fn name(&self) -> &str {
        &self.server_name
    }

    fn tools(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: self.tool_name.clone(),
            description: format!(
                "Send a prompt to the '{}' agent and wait for its final response",
                self.agent
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "message": {
                        "type": "string",
                        "description": "The prompt message to send to the agent"
                    },
                    "session_id": {
                        "type": "string",
                        "description": "An existing sub-agent session to continue"
                    }
                },
                "required": ["message"]
            }),
            idempotent_hint: false,
            read_only_hint: false,
            timeout_secs: None,
        }]
    }

    async fn invoke(
        &self,
        tool: &str,
        args: Value,
        _cancel: CancellationToken,
    ) -> Result<Value, ToolInvokeError> {
        if tool != self.tool_name {
            return Err(ToolInvokeError::Recoverable(format!(
                "unknown sub-agent tool: {tool}"
            )));
        }
        let args: PromptArgs = serde_json::from_value(args).map_err(|error| {
            ToolInvokeError::Recoverable(format!("invalid {tool} arguments: {error}"))
        })?;
        if args.message.trim().is_empty() {
            return Err(ToolInvokeError::Recoverable(
                "message must not be empty".to_string(),
            ));
        }

        let session = ThinClientSession::new(
            ThinClientConfig {
                cluster: self.cluster.clone(),
                agent: self.agent.clone(),
                session_id: args
                    .session_id
                    .filter(|session_id| !session_id.trim().is_empty()),
            },
            self.client.clone(),
            self.jetstream.clone(),
            harnx_core::abort::create_abort_signal(),
        )
        .await
        .map_err(|error| {
            ToolInvokeError::Recoverable(format!("create sub-agent session: {error:#}"))
        })?;
        let result = session
            .run_turn(&args.message, Arc::new(NullSink), None)
            .await
            .map_err(|error| {
                ToolInvokeError::Recoverable(format!("run sub-agent turn: {error:#}"))
            })?;
        let response = result.response.ok_or_else(|| {
            ToolInvokeError::Recoverable("sub-agent turn returned no final response".to_string())
        })?;
        Ok(json!({ "response": response }))
    }
}

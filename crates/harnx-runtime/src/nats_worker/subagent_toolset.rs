//! NATS toolset for nested sub-agent sessions.

use super::{publish_control_command, ControlCommand};
use crate::nats_client_session::{ThinClientConfig, ThinClientSession, ThinClientTurnResult};
use crate::nats_session_log::NatsSessionLog;
use async_nats::jetstream;
use async_trait::async_trait;
use harnx_core::event::{AgentEvent, AgentEventSink, AgentSource, TurnEvent};
use harnx_core::package_namespace::sanitize_for_tool_name;
use harnx_core::session::SessionLogEntry;
use harnx_toolset::{ToolInvokeError, ToolSpec, Toolset};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 300;
const DEFAULT_OPERATION_TIMEOUT_SECS: u64 = 3600;
const IDLE_TIMEOUT_ENV: &str = "HARNX_SUBAGENT_IDLE_TIMEOUT_SECS";
const OPERATION_TIMEOUT_ENV: &str = "HARNX_SUBAGENT_OPERATION_TIMEOUT_SECS";
// `_session_new` has no arguments, so a fixed bootstrap message creates the
// durable session and preserves blocking call-and-return semantics.
const SESSION_NEW_INITIAL_PROMPT: &str = "Start a new session.";

/// Timeout policy for blocking sub-agent turns.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SubagentTimeouts {
    idle: Duration,
    operation: Duration,
}

impl Default for SubagentTimeouts {
    fn default() -> Self {
        Self::from_env()
    }
}

impl SubagentTimeouts {
    fn from_env() -> Self {
        Self {
            idle: timeout_from_env(IDLE_TIMEOUT_ENV, DEFAULT_IDLE_TIMEOUT_SECS),
            operation: timeout_from_env(OPERATION_TIMEOUT_ENV, DEFAULT_OPERATION_TIMEOUT_SECS),
        }
    }

    #[cfg(test)]
    pub(crate) fn new(idle: Duration, operation: Duration) -> Self {
        Self { idle, operation }
    }
}

fn timeout_from_env(name: &str, default_secs: u64) -> Duration {
    let seconds = std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .unwrap_or(default_secs);
    Duration::from_secs(seconds)
}

/// Four-tool NATS adapter for one configured agent.
pub(crate) struct SubagentToolset {
    agent: String,
    cluster: String,
    server_name: String,
    client: async_nats::Client,
    jetstream: jetstream::Context,
    timeouts: SubagentTimeouts,
}

impl SubagentToolset {
    pub(crate) fn new(
        agent: impl Into<String>,
        cluster: impl Into<String>,
        client: async_nats::Client,
        jetstream: jetstream::Context,
    ) -> Self {
        Self::with_timeouts(
            agent,
            cluster,
            client,
            jetstream,
            SubagentTimeouts::default(),
        )
    }

    pub(crate) fn with_timeouts(
        agent: impl Into<String>,
        cluster: impl Into<String>,
        client: async_nats::Client,
        jetstream: jetstream::Context,
        timeouts: SubagentTimeouts,
    ) -> Self {
        let agent = agent.into();
        Self {
            server_name: sanitize_for_tool_name(&agent),
            agent,
            cluster: cluster.into(),
            client,
            jetstream,
            timeouts,
        }
    }

    fn tool_name(&self, method: &str) -> String {
        format!("{}_session_{method}", self.server_name)
    }

    async fn create_session(
        &self,
        session_id: Option<String>,
    ) -> Result<ThinClientSession, ToolInvokeError> {
        ThinClientSession::new(
            ThinClientConfig {
                cluster: self.cluster.clone(),
                agent: self.agent.clone(),
                session_id,
            },
            self.client.clone(),
            self.jetstream.clone(),
            harnx_core::abort::create_abort_signal(),
        )
        .await
        .map_err(|error| {
            ToolInvokeError::Recoverable(format!("create sub-agent session: {error:#}"))
        })
    }

    async fn run_prompt(
        &self,
        message: &str,
        session_id: Option<String>,
        parent_session_id: Option<String>,
        cancel: CancellationToken,
    ) -> Result<ThinClientTurnResult, ToolInvokeError> {
        let session = self.create_session(session_id).await?;
        let child_session_id = session.session_id().to_string();
        if let Some(parent_session_id) = parent_session_id {
            self.emit_parent_subagent_started(&parent_session_id, &child_session_id)
                .await?;
        }
        let (activity_tx, mut activity_rx) = watch::channel(0_u64);
        let sink = Arc::new(ActivitySink { activity_tx });
        let (cancel_tx, cancel_rx) = mpsc::channel(1);
        let run_turn = session.run_turn(message, sink, Some(cancel_rx));
        tokio::pin!(run_turn);
        let operation_timeout = tokio::time::sleep(self.timeouts.operation);
        let idle_timeout = tokio::time::sleep(self.timeouts.idle);
        tokio::pin!(operation_timeout);
        tokio::pin!(idle_timeout);

        loop {
            tokio::select! {
                result = &mut run_turn => {
                    let result = result.map_err(|error| {
                        ToolInvokeError::Recoverable(format!("run sub-agent turn: {error:#}"))
                    })?;
                    if result.was_cancelled || self.turn_has_cancel(&result).await {
                        return Err(ToolInvokeError::Recoverable(
                            "sub-agent turn was cancelled".to_string(),
                        ));
                    }
                    return Ok(result);
                }
                _ = cancel.cancelled() => {
                    let _ = cancel_tx.send(()).await;
                    let _ = (&mut run_turn).await;
                    return Err(ToolInvokeError::Fatal(
                        "sub-agent tool call aborted".to_string(),
                    ));
                }
                _ = &mut operation_timeout => {
                    self.cancel_child(&child_session_id).await;
                    return Err(ToolInvokeError::Recoverable(format!(
                        "sub-agent '{}' timed out during session prompt (overall timeout)",
                        self.agent,
                    )));
                }
                _ = &mut idle_timeout => {
                    self.cancel_child(&child_session_id).await;
                    return Err(ToolInvokeError::Recoverable(format!(
                        "sub-agent '{}' timed out during session prompt (idle timeout); the agent may have stopped making progress",
                        self.agent,
                    )));
                }
                activity = activity_rx.changed() => {
                    if activity.is_ok() {
                        idle_timeout.as_mut().reset(tokio::time::Instant::now() + self.timeouts.idle);
                    }
                }
            }
        }
    }

    async fn emit_parent_subagent_started(
        &self,
        parent_session_id: &str,
        child_session_id: &str,
    ) -> Result<(), ToolInvokeError> {
        let source = AgentSource {
            agent: self.agent.clone(),
            session_id: Some(child_session_id.to_string()),
            model: None,
        };
        let event = AgentEvent::sub_agent(
            source,
            AgentEvent::Turn(TurnEvent::SubAgentStarted {
                agent: self.agent.clone(),
                session_id: child_session_id.to_string(),
            }),
        );
        let stream_name = crate::nats_session_log::stream_name_for_session(parent_session_id);
        let after_seq = match self.jetstream.get_stream(&stream_name).await {
            Ok(mut stream) => stream
                .info()
                .await
                .map(|info| info.state.last_sequence)
                .unwrap_or(0),
            Err(_) => 0,
        };
        let envelope = crate::nats_event_sink::AdvisoryEnvelope::new(after_seq, event);
        self.client
            .publish(
                crate::nats_event_sink::events_subject(parent_session_id),
                envelope
                    .to_bytes()
                    .map_err(|error| ToolInvokeError::Recoverable(error.to_string()))?
                    .into(),
            )
            .await
            .map_err(|error| {
                ToolInvokeError::Recoverable(format!(
                    "publish sub-agent start event to parent session: {error}"
                ))
            })?;
        self.client.flush().await.map_err(|error| {
            ToolInvokeError::Recoverable(format!(
                "flush sub-agent start event to parent session: {error}"
            ))
        })
    }

    async fn turn_has_cancel(&self, result: &ThinClientTurnResult) -> bool {
        NatsSessionLog::new(self.jetstream.clone(), result.session_id.clone())
            .load_events_async()
            .await
            .is_ok_and(|events| {
                events.iter().any(|(seq, entry)| {
                    *seq > result.user_msg_seq && matches!(entry, SessionLogEntry::Cancel { .. })
                })
            })
    }

    async fn cancel_child(&self, session_id: &str) {
        if let Err(error) =
            publish_control_command(&self.client, session_id, &ControlCommand::Cancel).await
        {
            log::warn!("failed to cancel sub-agent session '{session_id}': {error:#}");
        }
    }

    async fn session_new(
        &self,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<Value, ToolInvokeError> {
        let args: NewSessionArgs = parse_args(&self.tool_name("new"), args)?;
        let result = self
            .run_prompt(
                SESSION_NEW_INITIAL_PROMPT,
                None,
                args.parent_session_id,
                cancel,
            )
            .await?;
        self.turn_result_value(&result)
    }

    async fn session_prompt(
        &self,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<Value, ToolInvokeError> {
        let args: PromptArgs = parse_args(&self.tool_name("prompt"), args)?;
        if args.message.trim().is_empty() {
            return Err(ToolInvokeError::Recoverable(
                "message must not be empty".to_string(),
            ));
        }
        let result = self
            .run_prompt(
                &args.message,
                normalize_session_id(args.session_id),
                args.parent_session_id,
                cancel,
            )
            .await?;
        self.turn_result_value(&result)
    }

    fn turn_result_value(&self, result: &ThinClientTurnResult) -> Result<Value, ToolInvokeError> {
        let response = require_response(result)?;
        let source = AgentSource {
            agent: self.agent.clone(),
            session_id: Some(result.session_id.clone()),
            model: None,
        };
        Ok(json!({
            "session_id": result.session_id,
            "response": response,
            "sub_agent": source,
        }))
    }

    async fn session_load(&self, args: Value) -> Result<Value, ToolInvokeError> {
        let args: SessionArgs = parse_args(&self.tool_name("load"), args)?;
        let session_id = required_session_id(args.session_id)?;
        let events = NatsSessionLog::new(self.jetstream.clone(), session_id.clone())
            .load_events_async()
            .await
            .map_err(|error| {
                ToolInvokeError::Recoverable(format!(
                    "load sub-agent session '{session_id}': {error:#}"
                ))
            })?;
        Ok(json!({ "session_id": session_id, "events": events }))
    }

    async fn session_cancel(&self, args: Value) -> Result<Value, ToolInvokeError> {
        let args: SessionArgs = parse_args(&self.tool_name("cancel"), args)?;
        let session_id = required_session_id(args.session_id)?;
        publish_control_command(&self.client, &session_id, &ControlCommand::Cancel)
            .await
            .map_err(|error| {
                ToolInvokeError::Recoverable(format!(
                    "cancel sub-agent session '{session_id}': {error:#}"
                ))
            })?;
        Ok(json!({ "session_id": session_id, "cancelled": true }))
    }
}

fn require_response(result: &ThinClientTurnResult) -> Result<&str, ToolInvokeError> {
    result.response.as_deref().ok_or_else(|| {
        ToolInvokeError::Recoverable("sub-agent turn returned no final response".to_string())
    })
}

fn normalize_session_id(session_id: Option<String>) -> Option<String> {
    session_id.filter(|session_id| !session_id.trim().is_empty())
}

fn required_session_id(session_id: String) -> Result<String, ToolInvokeError> {
    if session_id.trim().is_empty() {
        Err(ToolInvokeError::Recoverable(
            "session_id must not be empty".to_string(),
        ))
    } else {
        Ok(session_id)
    }
}

fn parse_args<T: for<'de> Deserialize<'de>>(tool: &str, args: Value) -> Result<T, ToolInvokeError> {
    serde_json::from_value(args)
        .map_err(|error| ToolInvokeError::Recoverable(format!("invalid {tool} arguments: {error}")))
}

struct ActivitySink {
    activity_tx: watch::Sender<u64>,
}

impl AgentEventSink for ActivitySink {
    fn emit(&self, _event: AgentEvent) {
        self.activity_tx.send_modify(|generation| *generation += 1);
    }
}

#[derive(Deserialize)]
struct NewSessionArgs {
    #[serde(default, rename = "__harnx_parent_session_id")]
    parent_session_id: Option<String>,
}

#[derive(Deserialize)]
struct PromptArgs {
    message: String,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default, rename = "__harnx_parent_session_id")]
    parent_session_id: Option<String>,
}

#[derive(Deserialize)]
struct SessionArgs {
    session_id: String,
}

#[async_trait]
impl Toolset for SubagentToolset {
    fn name(&self) -> &str {
        &self.server_name
    }

    fn tools(&self) -> Vec<ToolSpec> {
        tool_specs(&self.agent, &self.server_name, self.timeouts)
    }

    async fn invoke(
        &self,
        tool: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<Value, ToolInvokeError> {
        if tool == self.tool_name("new") {
            self.session_new(args, cancel).await
        } else if tool == self.tool_name("prompt") {
            self.session_prompt(args, cancel).await
        } else if tool == self.tool_name("load") {
            self.session_load(args).await
        } else if tool == self.tool_name("cancel") {
            self.session_cancel(args).await
        } else {
            Err(ToolInvokeError::Recoverable(format!(
                "unknown sub-agent tool: {tool}"
            )))
        }
    }
}

fn tool_specs(agent: &str, server_name: &str, timeouts: SubagentTimeouts) -> Vec<ToolSpec> {
    let request_timeout = timeouts.operation.as_secs().saturating_add(5);
    vec![
        ToolSpec {
                name: format!("{server_name}_session_new"),
                description: format!("Create a new session on the '{}' agent", agent),
                input_schema: json!({ "type": "object", "properties": {} }),
                idempotent_hint: false,
                read_only_hint: false,
                timeout_secs: Some(request_timeout),
            },
            ToolSpec {
                name: format!("{server_name}_session_prompt"),
                description: format!(
                    "Send a prompt to the '{}' agent. To continue a conversation, pass only the exact session_id returned by session_prompt or session_new. To start a new conversation, omit session_id; empty or whitespace-only values also start a new session. Do not invent a session ID.",
                    agent
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
                            "description": "To continue a conversation, use the exact session ID returned by session_prompt or session_new"
                        }
                    },
                    "required": ["message"]
                }),
                idempotent_hint: false,
                read_only_hint: false,
                timeout_secs: Some(request_timeout),
            },
            ToolSpec {
                name: format!("{server_name}_session_load"),
                description: format!(
                    "Load an existing session on the '{}' agent and resume its prior context",
                    agent
                ),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "session_id": {
                            "type": "string",
                            "description": "The session ID to load"
                        }
                    },
                    "required": ["session_id"]
                }),
                idempotent_hint: true,
                read_only_hint: true,
                timeout_secs: Some(60),
            },
            ToolSpec {
                name: format!("{server_name}_session_cancel"),
                description: format!("Cancel a running prompt on the '{}' agent", agent),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "session_id": {
                            "type": "string",
                            "description": "The session ID to cancel"
                        }
                    },
                    "required": ["session_id"]
                }),
                idempotent_hint: true,
                read_only_hint: false,
                timeout_secs: Some(60),
            },
        ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_four_agent_session_tools_with_acp_compatible_schemas() {
        let tools = tool_specs(
            "pkg/helper",
            "pkg__helper",
            SubagentTimeouts::new(Duration::from_secs(3), Duration::from_secs(7)),
        );
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            vec![
                "pkg__helper_session_new",
                "pkg__helper_session_prompt",
                "pkg__helper_session_load",
                "pkg__helper_session_cancel",
            ]
        );
        assert_eq!(
            tools[0].input_schema,
            json!({ "type": "object", "properties": {} })
        );
        assert_eq!(tools[1].input_schema["required"], json!(["message"]));
        assert_eq!(tools[2].input_schema["required"], json!(["session_id"]));
        assert_eq!(tools[3].input_schema["required"], json!(["session_id"]));
        assert_eq!(tools[0].timeout_secs, Some(12));
        assert_eq!(tools[1].timeout_secs, Some(12));
    }
}

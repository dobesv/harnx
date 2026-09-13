//! NATS toolset for nested sub-agent sessions.

mod termination;

use super::subagent_progress::SubagentProgressReporter;
use crate::nats_event_sink::NatsEventSink;
use crate::nats_session::{NatsSession, NatsSessionConfig, NatsTurnResult};
use crate::nats_session_log::NatsSessionLog;
use crate::nats_worker::SessionActivationRoute;
use crate::SynthesizedResult;
use async_nats::jetstream;
use async_trait::async_trait;
use harnx_core::event::{AgentEvent, AgentSource, SubAgentProgress, TurnEvent};
use harnx_core::package_namespace::sanitize_for_tool_name;
use harnx_core::session::SessionLogEntry;
use harnx_toolset::{
    ToolInvocation, ToolInvocationContext, ToolInvokeError, ToolSpec, Toolset,
    SUBAGENT_SESSION_CANCEL_TOOL, SUBAGENT_SESSION_LOAD_TOOL, SUBAGENT_SESSION_NEW_TOOL,
    SUBAGENT_SESSION_PROMPT_TOOL,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

// `_session_new` has no arguments, so a fixed bootstrap message creates the
// durable session and preserves blocking call-and-return semantics.
const SESSION_NEW_INITIAL_PROMPT: &str = "Start a new session.";
const SUBAGENT_PROGRESS_HEARTBEAT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub(crate) struct SubagentSessionRoute {
    cluster: String,
    activation: SessionActivationRoute,
}

impl SubagentSessionRoute {
    pub(crate) fn new(cluster: impl Into<String>, activation: SessionActivationRoute) -> Self {
        Self {
            cluster: cluster.into(),
            activation,
        }
    }

    fn session_config(&self, agent: &str, session_id: Option<String>) -> NatsSessionConfig {
        NatsSessionConfig {
            cluster: self.cluster.clone(),
            initializer: crate::SessionInitializer::named(
                agent,
                harnx_core::agent_config::AgentVariables::default(),
            ),
            session_id,
            activation_route: self.activation.clone(),
        }
    }
}

/// Four-tool NATS adapter for one configured agent.
pub(crate) struct SubagentToolset {
    agent: String,
    route: SubagentSessionRoute,
    server_name: String,
    client: async_nats::Client,
    jetstream: jetstream::Context,
    session_metadata: crate::nats_session_metadata::SessionMetadataStore,
    progress_heartbeat: Duration,
}

pub(crate) struct SubagentNats {
    client: async_nats::Client,
    jetstream: jetstream::Context,
    session_metadata: crate::nats_session_metadata::SessionMetadataStore,
}

impl SubagentNats {
    pub(crate) fn new(
        client: async_nats::Client,
        jetstream: jetstream::Context,
        session_metadata: crate::nats_session_metadata::SessionMetadataStore,
    ) -> Self {
        Self {
            client,
            jetstream,
            session_metadata,
        }
    }
}

struct SubagentStart<'a> {
    child_session_id: &'a str,
    invocation_id: &'a str,
    tool_call_id: Option<&'a str>,
}

struct ProgressReporterStart {
    child_session_id: String,
    parent_session_id: Option<String>,
    invocation_id: String,
    tool_call_id: Option<String>,
}

impl SubagentToolset {
    pub(crate) fn new(
        agent: impl Into<String>,
        route: SubagentSessionRoute,
        nats: SubagentNats,
    ) -> Self {
        let agent = agent.into();
        let server_name = agent
            .rsplit_once('/')
            .map_or(agent.as_str(), |(_, stem)| stem);
        Self {
            server_name: sanitize_for_tool_name(server_name),
            agent,
            route,
            client: nats.client,
            jetstream: nats.jetstream,
            session_metadata: nats.session_metadata,
            progress_heartbeat: SUBAGENT_PROGRESS_HEARTBEAT,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_progress_heartbeat(mut self, heartbeat: Duration) -> Self {
        self.progress_heartbeat = heartbeat;
        self
    }

    async fn create_session(
        &self,
        session_id: Option<String>,
        parent_session_id: Option<&str>,
    ) -> Result<NatsSession, ToolInvokeError> {
        let config = self.session_config(session_id, parent_session_id).await?;
        NatsSession::new(
            config,
            self.client.clone(),
            self.jetstream.clone(),
            harnx_core::abort::create_abort_signal(),
        )
        .await
        .map_err(|error| {
            ToolInvokeError::Recoverable(format!("create sub-agent session: {error:#}"))
        })
    }

    async fn session_config(
        &self,
        session_id: Option<String>,
        parent_session_id: Option<&str>,
    ) -> Result<crate::NatsSessionConfig, ToolInvokeError> {
        let mut config = self.route.session_config(&self.agent, session_id.clone());
        if session_id.is_none() {
            if let Some(parent_session_id) = parent_session_id {
                let context = self
                    .session_metadata
                    .get_tool_context(parent_session_id)
                    .await
                    .map_err(|error| {
                        ToolInvokeError::Recoverable(format!(
                            "load parent session tool context: {error:#}"
                        ))
                    })?;
                // Older sessions and direct Toolset callers may have no metadata record.
                // Treat that as an empty context so delegation remains rollout-compatible.
                if let Some(context) = context {
                    config.initializer = config.initializer.with_tool_context(context);
                }
            }
        }
        Ok(config)
    }

    async fn run_prompt(
        &self,
        params: termination::PromptParams<'_>,
    ) -> Result<CompletedSubagentTurn, ToolInvokeError> {
        termination::run_prompt(self, params).await
    }

    async fn start_progress_reporter(
        &self,
        start: ProgressReporterStart,
    ) -> Result<SubagentProgressReporter, ToolInvokeError> {
        let parent_sink = match start.parent_session_id {
            Some(parent_session_id) => {
                let sink = NatsEventSink::new(
                    self.client.clone(),
                    self.jetstream.clone(),
                    parent_session_id.clone(),
                )
                .await;
                self.emit_parent_subagent_started(
                    &sink,
                    &parent_session_id,
                    SubagentStart {
                        child_session_id: &start.child_session_id,
                        invocation_id: &start.invocation_id,
                        tool_call_id: start.tool_call_id.as_deref(),
                    },
                )
                .await?;
                Some(sink)
            }
            None => None,
        };
        Ok(SubagentProgressReporter::spawn(
            self.agent.clone(),
            start.child_session_id,
            start.invocation_id,
            parent_sink,
            self.progress_heartbeat,
        ))
    }

    async fn emit_parent_subagent_started(
        &self,
        parent_sink: &NatsEventSink,
        parent_session_id: &str,
        start: SubagentStart<'_>,
    ) -> Result<(), ToolInvokeError> {
        let SubagentStart {
            child_session_id,
            invocation_id,
            tool_call_id,
        } = start;
        let entry = SessionLogEntry::SubAgentStarted {
            agent: self.agent.clone(),
            session_id: child_session_id.to_string(),
            invocation_id: Some(invocation_id.to_string()),
            tool_call_id: tool_call_id.map(str::to_string),
            started_at: Some(chrono::Utc::now()),
        };
        let (_, inserted) = crate::nats_session::append_invocation_entry(
            &NatsSessionLog::new(self.jetstream.clone(), parent_session_id),
            &entry,
            invocation_id,
        )
        .await
        .map_err(|error| {
            ToolInvokeError::Recoverable(format!("persist sub-agent start: {error:#}"))
        })?;
        if !inserted {
            return Ok(());
        }
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
                invocation_id: Some(invocation_id.to_string()),
            }),
        );
        parent_sink.emit_required(event);
        parent_sink.flush().await.map_err(|error| {
            ToolInvokeError::Recoverable(format!(
                "publish sub-agent start event to parent session: {error:#}"
            ))
        })?;

        Ok(())
    }

    async fn turn_has_cancel(&self, result: &NatsTurnResult) -> bool {
        NatsSessionLog::new(self.jetstream.clone(), result.session_id.clone())
            .load_events_async()
            .await
            .is_ok_and(|events| {
                events.iter().any(|(seq, entry)| {
                    *seq > result.user_msg_seq && matches!(entry, SessionLogEntry::Cancel { .. })
                })
            })
    }

    async fn session_new(
        &self,
        args: Value,
        cancel: CancellationToken,
        context: ToolInvocationContext,
    ) -> Result<Value, ToolInvokeError> {
        let args: NewSessionArgs = parse_args(SUBAGENT_SESSION_NEW_TOOL, args)?;
        let result = self
            .run_prompt(termination::PromptParams {
                message: SESSION_NEW_INITIAL_PROMPT,
                session_id: None,
                parent_session_id: args.parent_session_id,
                tool_call_id: args.tool_call_id,
                timeout_secs: None,
                token_budget: None,
                cancel,
                context,
            })
            .await?;
        self.turn_result_value(&result)
    }

    async fn session_prompt(
        &self,
        args: Value,
        cancel: CancellationToken,
        context: ToolInvocationContext,
    ) -> Result<Value, ToolInvokeError> {
        let args: PromptArgs = parse_args(SUBAGENT_SESSION_PROMPT_TOOL, args)?;
        if args.message.trim().is_empty() {
            return Err(ToolInvokeError::Recoverable(
                "message must not be empty".to_string(),
            ));
        }
        let result = self
            .run_prompt(termination::PromptParams {
                message: &args.message,
                session_id: normalize_session_id(args.session_id),
                parent_session_id: args.parent_session_id,
                tool_call_id: args.tool_call_id,
                timeout_secs: args.timeout_secs,
                token_budget: args.token_budget,
                cancel,
                context,
            })
            .await?;
        self.turn_result_value(&result)
    }

    fn turn_result_value(
        &self,
        completed: &CompletedSubagentTurn,
    ) -> Result<Value, ToolInvokeError> {
        termination::result_value(self, completed)
    }

    async fn session_load(&self, args: Value) -> Result<Value, ToolInvokeError> {
        let args: SessionArgs = parse_args(SUBAGENT_SESSION_LOAD_TOOL, args)?;
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
        let args: SessionArgs = parse_args(SUBAGENT_SESSION_CANCEL_TOOL, args)?;
        let session_id = required_session_id(args.session_id)?;
        let session = self.create_session(Some(session_id.clone()), None).await?;
        let receipt = session
            .request_cancel(crate::nats_session::CancelRequest {
                expected_execution_id: args.expected_execution_id,
                retry: true,
            })
            .await
            .map_err(|error| {
                ToolInvokeError::Recoverable(format!(
                    "cancel sub-agent session '{session_id}': {error:#}"
                ))
            })?;
        let mut value = serde_json::to_value(receipt)
            .map_err(|error| ToolInvokeError::Fatal(error.to_string()))?;
        value["session_id"] = json!(session_id);
        Ok(value)
    }
}

struct CompletedSubagentTurn {
    session_id: String,
    result: Option<NatsTurnResult>,
    progress: SubAgentProgress,
    termination: Option<SynthesizedResult>,
}

fn subagent_turn_failed(result: &NatsTurnResult, cancelled: bool) -> bool {
    if cancelled {
        return true;
    }
    if result.error.is_some() {
        return true;
    }
    result.response.is_none()
}

fn require_response(result: &NatsTurnResult) -> Result<&str, ToolInvokeError> {
    if let Some(error) = &result.error {
        return Err(ToolInvokeError::Recoverable(
            termination::subagent_error_message(
                format_args!("sub-agent turn failed: {error}"),
                &result.session_id,
            ),
        ));
    }
    result.response.as_deref().ok_or_else(|| {
        ToolInvokeError::Recoverable(termination::subagent_error_message(
            "sub-agent turn returned no final response",
            &result.session_id,
        ))
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

#[derive(Deserialize)]
struct NewSessionArgs {
    #[serde(default, rename = "__harnx_parent_session_id")]
    parent_session_id: Option<String>,
    #[serde(default, rename = "__harnx_tool_call_id")]
    tool_call_id: Option<String>,
}

#[derive(Deserialize)]
struct PromptArgs {
    message: String,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    token_budget: Option<u64>,
    #[serde(default, rename = "__harnx_parent_session_id")]
    parent_session_id: Option<String>,
    #[serde(default, rename = "__harnx_tool_call_id")]
    tool_call_id: Option<String>,
}

#[derive(Deserialize)]
struct SessionArgs {
    session_id: String,
    #[serde(default)]
    expected_execution_id: Option<String>,
}

#[async_trait]
impl Toolset for SubagentToolset {
    fn can_replay(&self, tool: &str) -> bool {
        matches!(
            tool,
            SUBAGENT_SESSION_NEW_TOOL
                | SUBAGENT_SESSION_PROMPT_TOOL
                | SUBAGENT_SESSION_LOAD_TOOL
                | SUBAGENT_SESSION_CANCEL_TOOL
        )
    }

    async fn replay(&self, invocation: ToolInvocation) -> Result<Value, ToolInvokeError> {
        if invocation.tool == SUBAGENT_SESSION_CANCEL_TOOL
            && invocation.args["expected_execution_id"].is_null()
        {
            return Err(ToolInvokeError::Recoverable(
                "interrupted cancellation cannot be replayed without its original execution ID"
                    .into(),
            ));
        }
        // run_prompt binds a durable child handle and deduplicates its admitted
        // prompt by invocation identity, so this reattaches instead of redelegating.
        self.invoke_with_context(invocation).await
    }

    fn name(&self) -> &str {
        &self.server_name
    }

    fn tools(&self) -> Vec<ToolSpec> {
        tool_specs(&self.agent)
    }

    async fn invoke(
        &self,
        tool: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<Value, ToolInvokeError> {
        if tool == SUBAGENT_SESSION_NEW_TOOL {
            self.session_new(args, cancel, standalone_context()).await
        } else if tool == SUBAGENT_SESSION_PROMPT_TOOL {
            self.session_prompt(args, cancel, standalone_context())
                .await
        } else if tool == SUBAGENT_SESSION_LOAD_TOOL {
            self.session_load(args).await
        } else if tool == SUBAGENT_SESSION_CANCEL_TOOL {
            self.session_cancel(args).await
        } else {
            Err(ToolInvokeError::Recoverable(format!(
                "unknown sub-agent tool: {tool}"
            )))
        }
    }

    async fn invoke_with_context(
        &self,
        mut invocation: ToolInvocation,
    ) -> Result<Value, ToolInvokeError> {
        if matches!(
            invocation.tool.as_str(),
            SUBAGENT_SESSION_NEW_TOOL | SUBAGENT_SESSION_PROMPT_TOOL
        ) {
            if let (Some(parent), Some(object)) = (
                invocation.context.invoking_session_id.clone(),
                invocation.args.as_object_mut(),
            ) {
                object.insert(
                    "__harnx_parent_session_id".to_string(),
                    Value::String(parent),
                );
            }
        }
        match invocation.tool.as_str() {
            SUBAGENT_SESSION_NEW_TOOL => {
                self.session_new(invocation.args, invocation.cancel, invocation.context)
                    .await
            }
            SUBAGENT_SESSION_PROMPT_TOOL => {
                self.session_prompt(invocation.args, invocation.cancel, invocation.context)
                    .await
            }
            _ => {
                self.invoke(&invocation.tool, invocation.args, invocation.cancel)
                    .await
            }
        }
    }
}

fn tool_specs(agent: &str) -> Vec<ToolSpec> {
    vec![
        session_new_spec(agent),
        session_prompt_spec(agent),
        session_id_tool_spec(agent, SessionIdTool::Load),
        session_id_tool_spec(agent, SessionIdTool::Cancel),
    ]
}

/// A truncated session ID, so the call header stays one short line.
const SHORT_SESSION_ID: &str = "{{ args.session_id | truncate(8, end='') }}";

fn session_new_spec(agent: &str) -> ToolSpec {
    ToolSpec {
        cancellation_guarantee: Default::default(),
        name: SUBAGENT_SESSION_NEW_TOOL.to_string(),
        description: format!("Create a new session on the '{agent}' agent"),
        input_schema: json!({ "type": "object", "properties": {} }),
        idempotent_hint: false,
        read_only_hint: false,
        timeout_secs: None,
        meta: None,
    }
    .without_request_timeout()
    .with_call_template(&format!("@ {agent} new session"))
}

fn session_prompt_spec(agent: &str) -> ToolSpec {
    ToolSpec {
            cancellation_guarantee: Default::default(),
        name: SUBAGENT_SESSION_PROMPT_TOOL.to_string(),
        description: format!(
            "Send a prompt to the '{agent}' agent. To continue a conversation, pass only the exact session_id returned by session_prompt or session_new. To start a new conversation, omit session_id; empty or whitespace-only values also start a new session. Do not invent a session ID."
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
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Maximum invocation duration in seconds; 0 or unset means no time limit"
                },
                "token_budget": {
                    "type": "integer",
                    "description": "Maximum budgeted tokens for this invocation; 0 or unset means unlimited"
                }
            },
            "required": ["message"]
        }),
        idempotent_hint: false,
        read_only_hint: false,
        timeout_secs: None,
        meta: None,
    }
    .without_request_timeout()
    .with_call_template(&format!(
        "@ {agent}{{% if args.session_id %}} [{SHORT_SESSION_ID}]{{% endif %}}\n{{{{ args.message }}}}"
    ))
}

/// The two tools that take nothing but a session ID.
enum SessionIdTool {
    Load,
    Cancel,
}

impl SessionIdTool {
    fn tool_name(&self) -> &'static str {
        match self {
            Self::Load => SUBAGENT_SESSION_LOAD_TOOL,
            Self::Cancel => SUBAGENT_SESSION_CANCEL_TOOL,
        }
    }

    fn verb(&self) -> &'static str {
        match self {
            Self::Load => "load",
            Self::Cancel => "cancel",
        }
    }

    fn describe(&self, agent: &str) -> String {
        match self {
            Self::Load => format!(
                "Load an existing session on the '{agent}' agent and resume its prior context"
            ),
            Self::Cancel => format!("Cancel a running prompt on the '{agent}' agent"),
        }
    }

    /// Loading resumes context without touching the session; cancelling stops
    /// whatever it was doing.
    fn read_only(&self) -> bool {
        matches!(self, Self::Load)
    }
}

fn session_id_tool_spec(agent: &str, tool: SessionIdTool) -> ToolSpec {
    let verb = tool.verb();
    ToolSpec {
        cancellation_guarantee: Default::default(),
        name: tool.tool_name().to_string(),
        description: tool.describe(agent),
        input_schema: json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": format!("The session ID to {verb}")
                }
            },
            "required": ["session_id"]
        }),
        idempotent_hint: true,
        read_only_hint: tool.read_only(),
        timeout_secs: Some(60),
        meta: None,
    }
    .with_call_template(&format!("@ {agent} {verb} {SHORT_SESSION_ID}"))
}

fn standalone_context() -> ToolInvocationContext {
    ToolInvocationContext {
        call_id: uuid::Uuid::now_v7().to_string(),
        operation: None,
        invoking_session_id: None,
        capabilities: Default::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_four_agent_session_tools_with_stable_schemas() {
        let tools = tool_specs("pkg/helper");
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            vec![
                "session_new",
                "session_prompt",
                "session_load",
                "session_cancel",
            ]
        );
        assert_eq!(
            tools[0].input_schema,
            json!({ "type": "object", "properties": {} })
        );
        assert_eq!(tools[1].input_schema["required"], json!(["message"]));
        assert_eq!(
            tools[1].input_schema["properties"]["timeout_secs"]["type"],
            "integer"
        );
        assert_eq!(
            tools[1].input_schema["properties"]["token_budget"]["type"],
            "integer"
        );
        assert!(
            tools[1].input_schema["properties"]["timeout_secs"]["description"]
                .as_str()
                .is_some_and(|description| description.contains("seconds"))
        );
        assert!(
            tools[1].input_schema["properties"]["token_budget"]["description"]
                .as_str()
                .is_some_and(|description| description.contains("unlimited"))
        );
        assert_eq!(tools[2].input_schema["required"], json!(["session_id"]));
        assert_eq!(tools[3].input_schema["required"], json!(["session_id"]));
        assert_eq!(tools[0].timeout_secs, Some(0));
        assert_eq!(tools[1].timeout_secs, Some(0));
    }

    /// Without a `call_template` the client renders a YAML dump of the
    /// arguments, which for `session_prompt` means the whole prompt body.
    fn call_templates(agent: &str) -> Vec<String> {
        tool_specs(agent)
            .iter()
            .map(|tool| {
                tool.meta
                    .as_ref()
                    .and_then(|meta| meta.get("call_template"))
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| panic!("tool '{}' has no call_template", tool.name))
                    .to_string()
            })
            .collect()
    }

    /// A packaged agent shows its canonical `pkg/agent` name, not the sanitized
    /// tool-name form (`pkg__agent`) used for routing.
    #[test]
    fn every_session_tool_advertises_a_call_template() {
        assert_eq!(
            call_templates("pkg/helper"),
            vec![
                "@ pkg/helper new session",
                "@ pkg/helper{% if args.session_id %} [{{ args.session_id | truncate(8, end='') }}]{% endif %}\n{{ args.message }}",
                "@ pkg/helper load {{ args.session_id | truncate(8, end='') }}",
                "@ pkg/helper cancel {{ args.session_id | truncate(8, end='') }}",
            ]
        );
    }

    /// A top-level (non-packaged) agent name has no `/`, so it renders unchanged.
    #[test]
    fn call_templates_use_a_bare_agent_name_unchanged() {
        assert_eq!(
            call_templates("helper"),
            vec![
                "@ helper new session",
                "@ helper{% if args.session_id %} [{{ args.session_id | truncate(8, end='') }}]{% endif %}\n{{ args.message }}",
                "@ helper load {{ args.session_id | truncate(8, end='') }}",
                "@ helper cancel {{ args.session_id | truncate(8, end='') }}",
            ]
        );
    }

    #[test]
    fn local_subagent_sessions_reuse_the_parent_workers_target_route() {
        let activation = SessionActivationRoute::WorkerTargeted {
            session_scope: "__local__".to_string(),
            worker_id: "local-parent".to_string(),
        };
        let route = SubagentSessionRoute::new("__local__", activation.clone());

        let config = route.session_config("helper", Some("child".to_string()));

        assert_eq!(config.cluster, "__local__");
        assert_eq!(config.initializer.agent_name(), Some("helper"));
        assert_eq!(config.session_id.as_deref(), Some("child"));
        assert_eq!(config.activation_route, activation);
    }

    #[test]
    fn cloud_subagent_sessions_keep_cluster_shared_activation() {
        let route = SubagentSessionRoute::new("prod", SessionActivationRoute::ClusterShared);

        let config = route.session_config("helper", Some("child".to_string()));

        assert_eq!(config.cluster, "prod");
        assert_eq!(
            config.activation_route,
            SessionActivationRoute::ClusterShared
        );
    }
}

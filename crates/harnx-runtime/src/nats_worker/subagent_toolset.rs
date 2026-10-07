//! NATS toolset for nested sub-agent sessions.

mod termination;

#[cfg(test)]
mod interrupt_tests;

#[cfg(test)]
mod policy_tests;

use super::subagent_progress::{ReportedInvocation, SubagentProgressReporter};
use crate::nats_event_sink::NatsEventSink;
use crate::nats_session::{NatsSession, NatsSessionConfig, NatsTurnResult};
use crate::nats_session_log::NatsSessionLog;
use crate::nats_worker::SessionActivationRoute;
use crate::SynthesizedResult;
use async_nats::jetstream;
use async_trait::async_trait;
use harnx_blob_store::media::{get_media, optional_attachments_bucket};
use harnx_core::cid_url::CidUrl;
use harnx_core::event::SubAgentProgress;
use harnx_core::message::{ImageUrl, MessageContent, MessageContentPart};
use harnx_core::package_namespace::sanitize_for_tool_name;
use harnx_core::session::SessionLogEntry;
use harnx_toolset::{
    ToolInvocation, ToolInvocationContext, ToolInvokeError, ToolProgressKind, ToolSpec, Toolset,
    SUBAGENT_SESSION_CANCEL_TOOL, SUBAGENT_SESSION_LOAD_TOOL, SUBAGENT_SESSION_NEW_TOOL,
    SUBAGENT_SESSION_PROMPT_TOOL,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::HashSet, time::Duration};
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

    pub(crate) fn cluster(&self) -> &str {
        &self.cluster
    }

    pub(crate) fn activation_route(&self) -> &SessionActivationRoute {
        &self.activation
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
    replicas: usize,
    progress_heartbeat: Duration,
    lease_acquisition_timeout: Duration,
    run_policy: Option<TargetRunPolicy>,
    default_user_id: Option<String>,
}

pub(crate) struct SubagentNats {
    client: async_nats::Client,
    jetstream: jetstream::Context,
    session_metadata: crate::nats_session_metadata::SessionMetadataStore,
    replicas: usize,
    lease_acquisition_timeout: Duration,
}

impl SubagentNats {
    pub(crate) fn new(
        client: async_nats::Client,
        jetstream: jetstream::Context,
        session_metadata: crate::nats_session_metadata::SessionMetadataStore,
        replicas: usize,
    ) -> Self {
        Self {
            client,
            jetstream,
            session_metadata,
            replicas,
            lease_acquisition_timeout: crate::nats_session::LEASE_ACQUISITION_TIMEOUT,
        }
    }

    pub(crate) fn with_lease_acquisition_timeout(mut self, timeout: Duration) -> Self {
        self.lease_acquisition_timeout = timeout;
        self
    }
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
            replicas: nats.replicas,
            progress_heartbeat: SUBAGENT_PROGRESS_HEARTBEAT,
            lease_acquisition_timeout: nats.lease_acquisition_timeout,
            run_policy: None,
            default_user_id: None,
        }
    }

    pub(crate) fn with_run_policy(mut self, policy: TargetRunPolicy) -> Self {
        self.run_policy = Some(policy);
        self
    }

    pub(crate) fn with_default_user_id(mut self, user_id: Option<String>) -> Self {
        self.default_user_id = user_id;
        self
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
        tool_call_id: Option<&str>,
    ) -> Result<NatsSession, ToolInvokeError> {
        let config = self
            .session_config(session_id, parent_session_id, tool_call_id)
            .await?;
        NatsSession::new_with_resolved_options(
            config,
            self.replicas,
            self.client.clone(),
            self.jetstream.clone(),
            harnx_core::abort::create_abort_signal(),
            self.lease_acquisition_timeout,
        )
        .await
        .map_err(|error| {
            ToolInvokeError::Recoverable(format!("create sub-agent session: {error:#}"))
        })
    }

    /// `tool_call_id` is the id the PARENT's transcript gave this call, not
    /// the wire id dispatch minted for it. The ancestor check looks the link
    /// up in the parent's `ToolCalls` entries, which only ever carry the
    /// transcript id, so a wire id there could never match and would leave
    /// every child reading as "parent still waiting". A call with no
    /// transcript id at all cannot be found in the parent's log either, so it
    /// gets no link rather than one that can only answer wrongly.
    async fn session_config(
        &self,
        session_id: Option<String>,
        parent_session_id: Option<&str>,
        tool_call_id: Option<&str>,
    ) -> Result<crate::NatsSessionConfig, ToolInvokeError> {
        let mut config = self.route.session_config(&self.agent, session_id.clone());
        if let (Some(parent_session_id), Some(tool_call_id)) = (parent_session_id, tool_call_id) {
            config.initializer.parent = Some(crate::nats_session_metadata::ParentLink {
                session_id: parent_session_id.to_string(),
                tool_call_id: tool_call_id.to_string(),
            });
        }
        if session_id.is_none() {
            if let Some(parent_session_id) = parent_session_id {
                let parent =
                    self.session_metadata
                        .get(parent_session_id)
                        .await
                        .map_err(|error| {
                            ToolInvokeError::Recoverable(format!(
                                "load parent session metadata: {error:#}"
                            ))
                        })?;
                // Older sessions and direct Toolset callers may have no metadata record.
                // Treat that as an empty context so delegation remains rollout-compatible.
                if let Some(parent) = parent {
                    config.initializer = inherit_from_parent(config.initializer, &parent.metadata)?;
                }
            }
        }
        config.initializer = config
            .initializer
            .with_default_user_id(self.default_user_id.clone());
        Ok(config)
    }

    async fn run_prompt(
        &self,
        params: termination::PromptParams,
    ) -> Result<CompletedSubagentTurn, ToolInvokeError> {
        Box::pin(termination::run_prompt(self, params)).await
    }
    async fn compose_prompt_content(
        &self,
        mut message: String,
        attachments: Vec<String>,
    ) -> Result<MessageContent, ToolInvokeError> {
        let attachments = parse_attachment_urls(attachments)?;
        let needs_media = attachments
            .iter()
            .any(|(_, url)| matches!(url, CidUrl::Media { .. }));
        let store = if needs_media {
            optional_attachments_bucket(&self.jetstream)
                .await
                .map_err(attachment_store_error)?
        } else {
            None
        };
        let mut images = Vec::new();
        for (raw, url) in attachments {
            if matches!(url, CidUrl::Plan { .. }) {
                append_attachment_line(&mut message, &url);
                continue;
            }
            let store = store.as_ref().ok_or_else(|| attachment_not_found(&raw))?;
            let Some((_, mime_type)) = get_media(store, &url)
                .await
                .map_err(attachment_store_error)?
            else {
                return Err(attachment_not_found(&raw));
            };
            if is_image_mime(&mime_type) {
                images.push(MessageContentPart::ImageUrl {
                    image_url: ImageUrl {
                        url: url.to_string(),
                    },
                });
            } else {
                append_attachment_line(&mut message, &url);
            }
        }
        if images.is_empty() {
            return Ok(MessageContent::Text(message));
        }
        let mut parts = vec![MessageContentPart::Text { text: message }];
        parts.extend(images);
        Ok(MessageContent::Array(parts))
    }

    async fn start_progress_reporter(
        &self,
        start: ProgressReporterStart,
    ) -> SubagentProgressReporter {
        let parent_sink = match start.parent_session_id {
            Some(parent_session_id) => Some(
                NatsEventSink::new(
                    self.client.clone(),
                    self.jetstream.clone(),
                    parent_session_id,
                )
                .await,
            ),
            None => None,
        };
        SubagentProgressReporter::start(
            ReportedInvocation {
                agent: self.agent.clone(),
                session_id: start.child_session_id,
                invocation_id: start.invocation_id,
                tool_call_id: start.tool_call_id,
            },
            parent_sink,
            self.session_metadata.clone(),
            self.progress_heartbeat,
        )
        .await
    }

    async fn turn_has_cancel(&self, result: &NatsTurnResult) -> bool {
        NatsSessionLog::new(
            self.jetstream.clone(),
            harnx_core::session_identity::session_key(Some(&self.agent), &result.session_id),
        )
        .load_events_async()
        .await
        .is_ok_and(|events| {
            let end = crate::nats_session::invocation_terminal_seq(&events, result.user_msg_seq)
                .unwrap_or(u64::MAX);
            events.iter().any(|(seq, entry)| {
                *seq > result.user_msg_seq
                    && *seq <= end
                    && matches!(entry, SessionLogEntry::Cancel { .. })
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
                content: MessageContent::Text(SESSION_NEW_INITIAL_PROMPT.to_string()),
                session_id: None,
                parent_session_id: args.parent_session_id,
                tool_call_id: args.tool_call_id,
                timeout_secs: None,
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
        let args = parse_prompt_args(args)?;
        if args.message.trim().is_empty() {
            return Err(ToolInvokeError::Recoverable(
                "message must not be empty".to_string(),
            ));
        }
        let content = self
            .compose_prompt_content(args.message, args.attachments.unwrap_or_default())
            .await?;
        let result = self
            .run_prompt(termination::PromptParams {
                content,
                session_id: normalize_session_id(args.session_id),
                parent_session_id: args.parent_session_id,
                tool_call_id: args.tool_call_id,
                timeout_secs: args.timeout_secs,
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
        let events = NatsSessionLog::new(
            self.jetstream.clone(),
            harnx_core::session_identity::session_key(Some(&self.agent), &session_id),
        )
        .load_events_async()
        .await
        .map_err(|error| {
            ToolInvokeError::Recoverable(format!(
                "load sub-agent session '{session_id}': {error:#}"
            ))
        })?;
        Ok(json!({ "session_id": session_id, "events": events }))
    }

    async fn session_cancel(
        &self,
        args: Value,
        context: ToolInvocationContext,
    ) -> Result<Value, ToolInvokeError> {
        let args: SessionArgs = parse_args(SUBAGENT_SESSION_CANCEL_TOOL, args)?;
        let session_id = required_session_id(args.session_id)?;
        let storage = harnx_core::session_identity::session_key(Some(&self.agent), &session_id);
        let log = crate::nats_session_log::NatsSessionLog::new(self.jetstream.clone(), &storage);
        let entries = log
            .load_events_async()
            .await
            .map_err(|error| ToolInvokeError::Recoverable(error.to_string()))?;
        let checkpoint = match context.checkpoint.clone() {
            Some(saved) => saved,
            None => {
                let current = self
                    .session_metadata
                    .active_admission(&storage, &entries)
                    .await
                    .map_err(|error| ToolInvokeError::Recoverable(error.to_string()))?;
                let proposed = json!({ "target_invocation": current.as_ref().map(|admission| admission.invocation_id.as_str()) });
                match context.checkpoint_store.as_ref() {
                    Some(store) => store
                        .checkpoint(proposed)
                        .await
                        .map_err(|error| ToolInvokeError::Recoverable(error.to_string()))?,
                    None => proposed,
                }
            }
        };
        let prompt_seq = match checkpoint["target_invocation"].as_str() {
            Some(id) => self
                .session_metadata
                .invocation_prompt_seq(&storage, id, &entries)
                .await
                .map_err(|error| ToolInvokeError::Recoverable(error.to_string()))?,
            None => None,
        };
        let Some(prompt_seq) = prompt_seq else {
            return Ok(json!({"outcome": "idle", "session_id": session_id}));
        };
        let request = crate::nats_session::interrupt::InterruptRequest {
            session_id: harnx_core::session_identity::session_key(Some(&self.agent), &session_id),
            cluster: self.route.cluster().to_string(),
            replicas: self.replicas,
            cancellation_id: format!("tool-stop:{}", context.call_id),
            requested_by: "session_cancel".to_string(),
            reason: "cancelled via session_cancel tool".into(),
        };
        let outcome = crate::nats_session::interrupt::interrupt_invocation(
            &self.jetstream,
            &self.client,
            self.route.activation_route(),
            request,
            prompt_seq,
        )
        .await
        .map_err(|error| {
            ToolInvokeError::Recoverable(format!(
                "cancel sub-agent session '{session_id}': {error:#}"
            ))
        })?;
        let mut value = serde_json::to_value(outcome)
            .map_err(|error| ToolInvokeError::Fatal(error.to_string()))?;
        value["session_id"] = json!(session_id);
        Ok(value)
    }
}

fn parse_attachment_urls(
    attachments: Vec<String>,
) -> Result<Vec<(String, CidUrl)>, ToolInvokeError> {
    let mut seen = HashSet::new();
    let mut parsed = Vec::with_capacity(attachments.len());
    for raw in attachments {
        let url = CidUrl::parse(&raw).map_err(|error| {
            ToolInvokeError::Recoverable(format!("invalid attachment URL '{raw}': {error}"))
        })?;
        if seen.insert(raw.clone()) {
            parsed.push((raw, url));
        }
    }
    Ok(parsed)
}

fn append_attachment_line(message: &mut String, url: &CidUrl) {
    message.push_str("\nAttachment: ");
    message.push_str(&url.to_string());
}

fn is_image_mime(mime_type: &str) -> bool {
    mime_type
        .split(';')
        .next()
        .is_some_and(|base| base.trim().to_ascii_lowercase().starts_with("image/"))
}

fn attachment_store_error(error: anyhow::Error) -> ToolInvokeError {
    ToolInvokeError::Recoverable(format!("inspect attachment: {error:#}"))
}

fn attachment_not_found(url: &str) -> ToolInvokeError {
    ToolInvokeError::Recoverable(format!("attachment not found: {url}"))
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

/// Start a child with its parent's complete tool context and the properties
/// the parent marked inherit. Properties are descriptive, so a parent whose
/// properties can't be decoded still delegates; the child just starts
/// without them.
fn inherit_from_parent(
    initializer: crate::SessionInitializer,
    parent: &crate::nats_session_metadata::SessionMetadata,
) -> Result<crate::SessionInitializer, ToolInvokeError> {
    let tool_context = crate::nats_session_metadata::tool_context(parent).map_err(|error| {
        ToolInvokeError::Recoverable(format!("load parent session tool context: {error:#}"))
    })?;
    let properties = crate::nats_session_metadata::session_properties(parent)
        .map(|properties| properties.inherited())
        .unwrap_or_else(|error| {
            log::warn!(
                "sub-agent session starts without inherited properties: parent={} error={error:#}",
                parent.session_id
            );
            Default::default()
        });
    Ok(initializer
        .with_tool_context(tool_context)
        .with_properties(properties))
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

fn parse_prompt_args(args: Value) -> Result<PromptArgs, ToolInvokeError> {
    let parsed: PromptArgs = parse_args(SUBAGENT_SESSION_PROMPT_TOOL, args)?;
    crate::nats_session_metadata::EffectiveDeadline::resolve(
        Default::default(),
        None,
        crate::nats_session_metadata::CallTimeoutOverride::from_optional(parsed.timeout_secs),
        None,
        chrono::Utc::now(),
    )
    .map_err(|error| {
        ToolInvokeError::Recoverable(format!("invalid session_prompt arguments: {error}"))
    })?;
    Ok(parsed)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewSessionArgs {
    #[serde(default, rename = "__harnx_parent_session_id")]
    parent_session_id: Option<String>,
    #[serde(default, rename = "__harnx_tool_call_id")]
    tool_call_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptArgs {
    message: String,
    #[serde(default)]
    attachments: Option<Vec<String>>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "harnx_core::config_data::deserialize_timeout_override"
    )]
    timeout_secs: Option<u64>,
    #[serde(default, rename = "__harnx_parent_session_id")]
    parent_session_id: Option<String>,
    #[serde(default, rename = "__harnx_tool_call_id")]
    tool_call_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionArgs {
    session_id: String,
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
        // Cancel replays retain the checkpointed invocation target, so they
        // cannot interrupt a later independent run in the same conversation. `run_prompt` binds a
        // durable child handle and deduplicates its prompt by invocation
        // identity, so that reattaches instead of redelegating.
        self.invoke_with_context(invocation).await
    }

    fn name(&self) -> &str {
        &self.server_name
    }

    fn tools(&self) -> Vec<ToolSpec> {
        tool_specs_with_policy(&self.agent, self.run_policy.as_ref())
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
            self.session_cancel(args, standalone_context()).await
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
            SUBAGENT_SESSION_CANCEL_TOOL => {
                self.session_cancel(invocation.args, invocation.context)
                    .await
            }
            _ => {
                self.invoke(&invocation.tool, invocation.args, invocation.cancel)
                    .await
            }
        }
    }

    /// Interrupt the child session an orphaned `session_prompt`/`session_new`
    /// call was running, by appending a `Cancel` to the CHILD's own log. Never
    /// created (no checkpoint recorded yet): nothing to interrupt.
    async fn cancel(&self, invocation: ToolInvocation) -> Result<(), ToolInvokeError> {
        let Some(child) = invocation
            .context
            .checkpoint
            .as_ref()
            .and_then(|checkpoint| checkpoint["session_id"].as_str())
        else {
            return Ok(());
        };
        let parent = invocation
            .context
            .invoking_session_id
            .clone()
            .unwrap_or_default();
        let request = crate::nats_session::interrupt::InterruptRequest {
            session_id: harnx_core::session_identity::session_key(Some(&self.agent), child),
            cluster: self.route.cluster().to_string(),
            replicas: self.replicas,
            cancellation_id: uuid::Uuid::now_v7().to_string(),
            requested_by: format!("parent:{parent}"),
            reason: "parent interrupted".into(),
        };
        let entries = crate::nats_session_log::NatsSessionLog::new(
            self.jetstream.clone(),
            &request.session_id,
        )
        .load_events_async()
        .await
        .map_err(|error| ToolInvokeError::Recoverable(error.to_string()))?;
        let Some(prompt_seq) = self
            .session_metadata
            .invocation_prompt_seq(&request.session_id, &invocation.context.call_id, &entries)
            .await
            .map_err(|error| ToolInvokeError::Recoverable(error.to_string()))?
        else {
            return Ok(());
        };
        crate::nats_session::interrupt::interrupt_invocation(
            &self.jetstream,
            &self.client,
            self.route.activation_route(),
            request,
            prompt_seq,
        )
        .await
        .map(drop)
        .map_err(|error| {
            ToolInvokeError::Recoverable(format!("interrupt sub-agent '{child}': {error:#}"))
        })
    }
}

#[cfg(test)]
fn tool_specs(agent: &str) -> Vec<ToolSpec> {
    tool_specs_with_policy(agent, None)
}

fn tool_specs_with_policy(agent: &str, policy: Option<&TargetRunPolicy>) -> Vec<ToolSpec> {
    vec![
        session_new_spec(agent, policy),
        session_prompt_spec(agent, policy),
        session_id_tool_spec(agent, SessionIdTool::Load),
        session_id_tool_spec(agent, SessionIdTool::Cancel),
    ]
}

/// Resolved on the publishing worker, never from the caller's active agent.
#[derive(Clone, Debug)]
pub(crate) struct TargetRunPolicy {
    timeout_secs: u64,
    source: crate::nats_session_metadata::RunLimitsPolicySource,
}

impl TargetRunPolicy {
    pub(crate) fn resolve(config: &crate::config::Config, agent: &str) -> anyhow::Result<Self> {
        // retrieve_agent applies the same package patches used at worker admission.
        let target = config.retrieve_agent(agent)?;
        let admitted_at = chrono::Utc::now();
        let resolved = config.resolve_run_deadline(
            Some(&target),
            crate::nats_session_metadata::CallTimeoutOverride::Omitted,
            None,
            admitted_at,
        )?;
        Ok(Self {
            timeout_secs: u64::try_from(
                (resolved.deadline.ok_or_else(|| {
                    anyhow::anyhow!("resolved target policy has no finite deadline")
                })? - admitted_at)
                    .num_seconds(),
            )?,
            source: resolved.source,
        })
    }
}

fn allowance_description(policy: Option<&TargetRunPolicy>) -> String {
    match policy {
        Some(TargetRunPolicy {
            timeout_secs: seconds,
            source,
        }) => {
            use crate::nats_session_metadata::RunLimitsPolicySource;
            let source = match source {
                RunLimitsPolicySource::GlobalDefault => "global policy",
                RunLimitsPolicySource::TargetAgent { .. } => "target policy",
                RunLimitsPolicySource::ExplicitOverride => "call override",
                RunLimitsPolicySource::InheritedFrom { .. } => "inherited policy",
            };
            format!("Target local allowance: {seconds} seconds ({source}).")
        }
        None => {
            "Target configured local allowance unavailable; target worker resolves it at admission."
                .into()
        }
    }
}

fn policy_description(policy: Option<&TargetRunPolicy>) -> String {
    let allowance = allowance_description(policy);
    format!("Omit or pass zero unless a specific deadline is needed. {allowance} Omitted, null, zero or negative inherits target policy (fallback: 86400 seconds / 24 hours). Positive seconds override local allowance; inherited deadlines can shorten it.")
}

/// A truncated session ID, so the call header stays one short line.
const SHORT_SESSION_ID: &str = "{{ args.session_id | truncate(8, end='') }}";

fn session_new_spec(agent: &str, policy: Option<&TargetRunPolicy>) -> ToolSpec {
    ToolSpec {
        cancellation_guarantee: Default::default(),
        name: SUBAGENT_SESSION_NEW_TOOL.to_string(),
        description: format!(
            "Create a new session on the '{agent}' agent. {}",
            allowance_description(policy) + " Uses target policy and the inherited deadline, which can shorten the allowance."
        ),
        input_schema: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        idempotent_hint: false,
        read_only_hint: false,
        timeout_secs: None,
        meta: None,
    }
    .without_request_timeout()
    .with_call_template(&format!("@ {agent} new session"))
    .with_kind(ToolProgressKind::Other)
}

fn session_prompt_spec(agent: &str, policy: Option<&TargetRunPolicy>) -> ToolSpec {
    ToolSpec {
            cancellation_guarantee: Default::default(),
        name: SUBAGENT_SESSION_PROMPT_TOOL.to_string(),
        description: format!(
            "Send a prompt to the '{agent}' agent. Omit session_id (or pass an empty/whitespace value) to start a new session with a generated ID — do this unless you are continuing an earlier session. To continue a session, pass the exact session_id returned by a prior session_prompt or session_new call. Session IDs are case-sensitive and local to this agent. Do not invent a session ID. Sub-agents return files by including cid: attachment URLs in their reply. {}", policy_description(policy)
        ),
        input_schema: json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "The prompt message to send to the agent"
                },
                "attachments": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional list of cid: attachment URLs to pass to the sub-agent"
                },
                "session_id": {
                    "type": "string",
                    "description": "Optional. Omit (or pass an empty/whitespace value) to start a new session with a generated ID; this is the default. To continue an earlier session, pass the exact session_id returned by a prior session_prompt or session_new call. IDs are case-sensitive and local to this agent. Do not invent a session ID."
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": policy_description(policy)
                }
            },
            "required": ["message"],
            "additionalProperties": false
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
    .with_kind(ToolProgressKind::Execute)
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
    let kind = match tool {
        SessionIdTool::Load => ToolProgressKind::Read,
        SessionIdTool::Cancel => ToolProgressKind::Delete,
    };
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
            "required": ["session_id"],
            "additionalProperties": false
        }),
        idempotent_hint: true,
        read_only_hint: tool.read_only(),
        timeout_secs: Some(60),
        meta: None,
    }
    .with_call_template(&format!("@ {agent} {verb} {SHORT_SESSION_ID}"))
    .with_kind(kind)
}

fn standalone_context() -> ToolInvocationContext {
    ToolInvocationContext {
        call_id: uuid::Uuid::now_v7().to_string(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn child_without_parent_identity_uses_worker_route_default() {
        harnx_core::require_nextest();
        let Some((url, mut nats, _store_dir)) = super::super::tests::spawn_test_nats().await else {
            return;
        };
        let toolset =
            std::sync::Arc::try_unwrap(super::super::tests::test_subagent_toolset(&url).await)
                .ok()
                .expect("test owns the toolset")
                .with_default_user_id(Some("cluster-default".into()));
        let config = toolset.session_config(None, None, None).await.unwrap();
        assert_eq!(
            config.initializer.properties.text("user_id"),
            Some("cluster-default")
        );
        let id = crate::utils::session_name::reserve_invocation_session_id(
            &toolset.session_metadata,
            &config.initializer,
            "new-child",
            1_735_689_600_000,
        )
        .await
        .unwrap();
        let child = toolset.create_session(Some(id), None, None).await.unwrap();
        let record = child
            .metadata_store()
            .get(child.storage_key())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            crate::nats_session_metadata::session_properties(&record.metadata)
                .unwrap()
                .text("user_id"),
            Some("cluster-default")
        );
        let _ = nats.kill();
        let _ = nats.wait();
    }

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
            json!({ "type": "object", "properties": {}, "additionalProperties": false })
        );
        assert_eq!(tools[1].input_schema["required"], json!(["message"]));
        assert_eq!(
            tools[1].input_schema["properties"]["attachments"]["type"],
            "array"
        );
        assert_eq!(
            tools[1].input_schema["properties"]["attachments"]["items"]["type"],
            "string"
        );
        assert!(tools[1].description.contains("cid: attachment URLs"));
        assert_eq!(
            tools[1].input_schema["properties"]["timeout_secs"]["type"],
            "integer"
        );
        assert!(tools[1].input_schema["properties"]
            .get("token_budget")
            .is_none());
        assert!(tools[1].input_schema["properties"]["timeout_secs"]
            .get("minimum")
            .is_none());
        assert_eq!(tools[2].input_schema["required"], json!(["session_id"]));
        assert_eq!(tools[3].input_schema["required"], json!(["session_id"]));
        assert_eq!(tools[0].timeout_secs, Some(0));
        assert_eq!(tools[1].timeout_secs, Some(0));
    }

    /// The `session_prompt` descriptions must steer models toward omitting
    /// `session_id` and must not suggest inventing one (regression guard for
    /// GitHub issue #1993 / the wording changed in #1894).
    #[test]
    fn session_prompt_descriptions_discourage_invented_ids() {
        let tools = tool_specs("pkg/helper");
        let prompt = &tools[1];

        let tool_description = prompt.description.as_str();
        let param_description = prompt.input_schema["properties"]["session_id"]["description"]
            .as_str()
            .expect("session_id description should be a string");

        for description in [tool_description, param_description] {
            assert!(
                description.contains("Do not invent"),
                "description should warn against inventing IDs: {description:?}"
            );
            assert!(
                description.contains("Omit") || description.contains("omit"),
                "description should tell the model it can omit session_id: {description:?}"
            );
            assert!(
                !description.contains("review-12345"),
                "description should not suggest a made-up example ID: {description:?}"
            );
            assert!(
                !description.to_lowercase().contains("unused id"),
                "description should not invite supplying an unused ID: {description:?}"
            );
        }
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

    /// The call templates every agent's four session tools should advertise,
    /// with `display` rendered into the leading `@ <agent>` mention.
    fn expected_call_templates(display: &str) -> Vec<String> {
        [
            format!("@ {display} new session"),
            format!(
                "@ {display}{{% if args.session_id %}} [{{{{ args.session_id | truncate(8, end='') }}}}]{{% endif %}}\n{{{{ args.message }}}}"
            ),
            format!("@ {display} load {{{{ args.session_id | truncate(8, end='') }}}}"),
            format!("@ {display} cancel {{{{ args.session_id | truncate(8, end='') }}}}"),
        ]
        .to_vec()
    }

    /// A packaged agent shows its canonical `pkg/agent` name, not the sanitized
    /// tool-name form (`pkg__agent`) used for routing.
    #[test]
    fn every_session_tool_advertises_a_call_template() {
        assert_eq!(
            call_templates("pkg/helper"),
            expected_call_templates("pkg/helper")
        );
    }

    /// A top-level (non-packaged) agent name has no `/`, so it renders unchanged.
    #[test]
    fn call_templates_use_a_bare_agent_name_unchanged() {
        assert_eq!(call_templates("helper"), expected_call_templates("helper"));
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

    #[test]
    fn all_subagent_tools_declare_correct_kind() {
        let tools = tool_specs("pkg/helper");
        let tool_map: std::collections::HashMap<_, _> =
            tools.iter().map(|s| (s.name.as_str(), s)).collect();

        assert_eq!(
            tool_map.get(SUBAGENT_SESSION_NEW_TOOL).unwrap().kind(),
            Some(ToolProgressKind::Other),
            "'session_new' should have Other kind"
        );
        assert_eq!(
            tool_map.get(SUBAGENT_SESSION_PROMPT_TOOL).unwrap().kind(),
            Some(ToolProgressKind::Execute),
            "'session_prompt' should have Execute kind"
        );
        assert_eq!(
            tool_map.get(SUBAGENT_SESSION_LOAD_TOOL).unwrap().kind(),
            Some(ToolProgressKind::Read),
            "'session_load' should have Read kind"
        );
        assert_eq!(
            tool_map.get(SUBAGENT_SESSION_CANCEL_TOOL).unwrap().kind(),
            Some(ToolProgressKind::Delete),
            "'session_cancel' should have Delete kind"
        );
    }
}

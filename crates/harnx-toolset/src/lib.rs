//! Shared toolset contract and transport-independent protocol types.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Environment variable carrying the tool server's package name.
pub const HARNX_SERVER_PACKAGE: &str = "HARNX_SERVER_PACKAGE";
/// Environment variable carrying the tool server's config-file stem.
pub const HARNX_SERVER_CONFIG: &str = "HARNX_SERVER_CONFIG";

/// Build the wire identity for a tool server.
pub fn server_identity_token(package: Option<&str>, config: &str, server: &str) -> String {
    format!("{}__{config}__{server}", package.unwrap_or_default())
}

/// Header carrying the request's idempotency key.
pub const HDR_IDEMPOTENCY_KEY: &str = "Idempotency-Key";
/// Header carrying the tool call ID.
pub const HDR_CALL_ID: &str = "X-Harnx-Call-Id";
/// Header carrying the worker instance ID.
pub const HDR_INSTANCE_ID: &str = "X-Harnx-Instance-Id";
/// Header carrying the payload media type.
pub const HDR_CONTENT_TYPE: &str = "Content-Type";

/// Whether dropping a per-call future guarantees that all its work stops.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancellationGuarantee {
    HardOnDrop,
    #[default]
    Cooperative,
}

mod cancellation;
pub use cancellation::*;
mod progress;
pub use progress::*;
pub mod cleanup;

/// Schema and execution hints for one tool.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    #[serde(default)]
    pub cancellation_guarantee: CancellationGuarantee,
    pub idempotent_hint: bool,
    pub read_only_hint: bool,
    /// Request/reply timeout advertised to transport clients, in seconds.
    /// Missing values use the client's default backstop for older registrations;
    /// zero disables the elapsed-time deadline so clients rely on cancellation
    /// and server-liveness detection instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// Tool `_meta` as in-house JSON, including optional display templates.
    /// Missing values indicate tools without `call_template`, `result_template`, or other metadata.
    ///
    /// This is the only place a client looks for display templates.
    /// `harnx_runtime::nats_tool_provider` reads `call_template` /
    /// `result_template` out of here to build the `ToolDeclaration`, and
    /// `harnx_toolset_server::run_toolset_main` rebuilds the MCP `list_tools`
    /// response from these specs too. A template attached only to a server
    /// crate's own rmcp `ServerHandler` therefore reaches nobody, and the tool
    /// call renders as a raw YAML dump of its arguments. Use
    /// [`ToolSpec::with_call_template`] / [`ToolSpec::with_result_template`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Map<String, serde_json::Value>>,
}

/// `_meta` key holding one of the client's display templates.
enum TemplateKey {
    Call,
    Result,
}

impl TemplateKey {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Call => "call_template",
            Self::Result => "result_template",
        }
    }
}

impl ToolSpec {
    /// Disable the transport's elapsed-time request deadline for this tool.
    ///
    /// Long-running tools should use this only when their transport can detect
    /// server loss independently, so a vanished server does not strand callers.
    #[must_use]
    pub fn without_request_timeout(mut self) -> Self {
        self.timeout_secs = Some(0);
        self
    }

    /// Attach the template the client uses to render the tool call header.
    #[must_use]
    pub fn with_call_template(self, template: &str) -> Self {
        self.with_template(TemplateKey::Call, template)
    }

    /// Attach the template the client uses to render the tool result.
    #[must_use]
    pub fn with_result_template(self, template: &str) -> Self {
        self.with_template(TemplateKey::Result, template)
    }

    fn with_template(mut self, key: TemplateKey, template: &str) -> Self {
        self.meta.get_or_insert_with(serde_json::Map::new).insert(
            key.as_str().to_string(),
            Value::String(template.to_string()),
        );
        self
    }
}

/// Terminal for this call: the session was interrupted before the tool
/// produced a result. Not a model-recoverable tool failure.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterruptedCall {
    /// The cancellation that caused the interruption, when the call knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancellation_id: Option<String>,
    pub reason: String,
}

impl fmt::Display for InterruptedCall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.cancellation_id {
            Some(id) => write!(
                f,
                "tool call interrupted ({}, cancellation {id})",
                self.reason
            ),
            None => write!(f, "tool call interrupted ({})", self.reason),
        }
    }
}

/// Error returned directly by a [`Toolset`] implementation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolInvokeError {
    Recoverable(String),
    Fatal(String),
    Interrupted(InterruptedCall),
}

impl fmt::Display for ToolInvokeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Recoverable(message) | Self::Fatal(message) => message.fmt(f),
            Self::Interrupted(interrupted) => interrupted.fmt(f),
        }
    }
}

impl std::error::Error for ToolInvokeError {}

/// Durable storage for one call's checkpoint handle, so an orphaned call can
/// later be cancelled without the process that started it.
///
/// Implementations back this with whatever the tool server already uses for
/// durability (KV, a file, a database row); [`Toolset`] implementations only
/// ever see the opaque [`Value`] they wrote.
#[async_trait]
pub trait CheckpointStore: Send + Sync {
    /// First writer wins; returns the stored value.
    async fn checkpoint(&self, value: Value) -> anyhow::Result<Value>;
}

/// Transport-provided facts about one tool invocation.
///
/// These values are trusted infrastructure context, not model-controlled tool
/// arguments. Toolsets that do not need them can continue implementing
/// [`Toolset::invoke`] only.
#[derive(Clone, Default)]
pub struct ToolInvocationContext {
    pub call_id: String,
    pub invoking_session_id: Option<String>,
    pub capabilities: BTreeSet<String>,
    /// Handle for a checkpoint recorded on a prior attempt, if any. Set on
    /// replay so the tool can resume instead of restarting from scratch.
    pub checkpoint: Option<Value>,
    /// Where to record this call's checkpoint handle, so [`Toolset::cancel`]
    /// can act on it after the original invocation is gone. Absent when the
    /// transport does not support checkpointing.
    pub checkpoint_store: Option<Arc<dyn CheckpointStore>>,
    /// Call-bound live progress sink. Defaults to a no-op for transports and
    /// callers that do not negotiate progress support.
    pub progress: ToolProgressHandle,
}

impl fmt::Debug for ToolInvocationContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolInvocationContext")
            .field("call_id", &self.call_id)
            .field("invoking_session_id", &self.invoking_session_id)
            .field("capabilities", &self.capabilities)
            .field("checkpoint", &self.checkpoint)
            .field(
                "checkpoint_store",
                &if self.checkpoint_store.is_some() {
                    "<set>"
                } else {
                    "<unset>"
                },
            )
            .field("progress", &self.progress)
            .finish()
    }
}

/// One tool invocation, including its transport-attested context and cancellation signal.
pub struct ToolInvocation {
    pub tool: String,
    pub args: Value,
    pub context: ToolInvocationContext,
    pub cancel: CancellationToken,
}

/// Collection of tools hosted by one tool server.
#[async_trait]
pub trait Toolset: Send + Sync {
    fn name(&self) -> &str;

    /// Default TCP port for the MCP Streamable HTTP transport.
    fn default_mcp_http_port(&self) -> u16 {
        3000
    }

    fn tools(&self) -> Vec<ToolSpec>;
    async fn invoke(
        &self,
        tool: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<Value, ToolInvokeError>;

    /// Invoke a tool with transport-attested invocation context.
    async fn invoke_with_context(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Value, ToolInvokeError> {
        self.invoke(&invocation.tool, invocation.args, invocation.cancel)
            .await
    }

    /// Permit a replacement server to own this invocation and call `replay`.
    /// Stateful tools override both methods to recover their original durable
    /// job; the default policy uses the tool server's repetition hints.
    fn can_replay(&self, tool: &str) -> bool {
        self.tools()
            .iter()
            .any(|spec| spec.name == tool && (spec.idempotent_hint || spec.read_only_hint))
    }

    /// Recover the original invocation after its caller/worker restarted.
    /// The invocation identity is unchanged. Implementations may reconnect to a
    /// durable job; they must not start another non-idempotent operation.
    /// The default retries only tools whose advertised hints permit repetition.
    async fn replay(&self, invocation: ToolInvocation) -> Result<Value, ToolInvokeError> {
        if self.can_replay(&invocation.tool) {
            self.invoke_with_context(invocation).await
        } else {
            Err(ToolInvokeError::Recoverable(
                "tool response lost (session was interrupted before results were persisted); this tool cannot replay the interrupted operation".into(),
            ))
        }
    }

    /// Cancel a call this process is not running (its original invocation is
    /// gone). `invocation.context.checkpoint` carries the handle the tool
    /// recorded, if any. Must be idempotent. Default: nothing to do.
    async fn cancel(&self, _invocation: ToolInvocation) -> Result<(), ToolInvokeError> {
        Ok(())
    }
}

/// Request body for one tool invocation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRequest {
    /// Set when this request replays a call whose original result was never
    /// observed. Preserves call_id/operation_id and never silently falls
    /// through to a normal invoke.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay: Option<ReplayAttempt>,
    pub operation_id: String,
    /// Wire id of this invocation, minted per dispatch attempt. The tool
    /// server journals the call under it and cancels are addressed by it, so
    /// it is never the transcript's tool-call id: a retried or replayed call
    /// gets a new `call_id` while its `tool_call_id` stays the same.
    pub call_id: String,
    pub tool: String,
    pub args: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// The transcript's tool-call id (`ToolCall.id`) this invocation answers,
    /// when the caller has one. Wind-up and replay resolve a journal row by
    /// `(session, tool round, tool_call_id)`, never by `call_id`, because only
    /// the transcript id survives a worker restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Additive capabilities understood by the caller. An absent field means
    /// private result metadata must not be returned.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub capabilities: BTreeSet<String>,
}

/// One attempt to replay a call whose original result was never observed,
/// rather than that call's first invocation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayAttempt {
    pub attempt: u32,
    pub requested_by: String,
}

/// Raw tool name for creating a sub-agent session.
pub const SUBAGENT_SESSION_NEW_TOOL: &str = "session_new";
/// Raw tool name for prompting a sub-agent session.
pub const SUBAGENT_SESSION_PROMPT_TOOL: &str = "session_prompt";
/// Raw tool name for loading a sub-agent session.
pub const SUBAGENT_SESSION_LOAD_TOOL: &str = "session_load";
/// Raw tool name for cancelling a sub-agent session.
pub const SUBAGENT_SESSION_CANCEL_TOOL: &str = "session_cancel";

/// Minimum elapsed time in milliseconds before showing a tool-call timer.
/// Tools running less than this duration show no elapsed-time display.
pub const TOOL_TIMER_MIN_ELAPSED_MS: u64 = 5_000;

/// Interval in milliseconds for CLI tool-call "still running" notices.
/// In append-only mode, a notice is printed each time this interval elapses.
pub const TOOL_TIMER_NOTICE_INTERVAL_MS: u64 = 10_000;

/// Returns `true` if the given tool name is a sub-agent launcher.
///
/// Launcher tools are `session_new` and `session_prompt` (and their agent/package-prefixed forms).
/// They are excluded from the generic tool-call timer because they already emit periodic progress.
///
/// # Examples
/// ```
/// # use harnx_toolset::is_subagent_launcher;
/// assert!(is_subagent_launcher("session_new"));
/// assert!(is_subagent_launcher("session_prompt"));
/// assert!(is_subagent_launcher("oracle_session_prompt"));
/// assert!(is_subagent_launcher("pantheon__oracle_session_prompt"));
///
/// assert!(!is_subagent_launcher("session_load"));
/// assert!(!is_subagent_launcher("session_cancel"));
/// assert!(!is_subagent_launcher("read_file"));
/// assert!(!is_subagent_launcher(""));
/// ```
pub fn is_subagent_launcher(name: &str) -> bool {
    name == SUBAGENT_SESSION_NEW_TOOL
        || name == SUBAGENT_SESSION_PROMPT_TOOL
        || name.ends_with(&format!("_{}", SUBAGENT_SESSION_NEW_TOOL))
        || name.ends_with(&format!("_{}", SUBAGENT_SESSION_PROMPT_TOOL))
}

/// Serializable error returned in a [`ToolReply`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "message", rename_all = "snake_case")]
pub enum ToolErrorPayload {
    Recoverable(String),
    Fatal(String),
    Interrupted(InterruptedCall),
}

/// Reply body for one tool invocation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolReply {
    pub call_id: String,
    pub result: Result<Value, ToolErrorPayload>,
    /// Latest bounded progress state. Kept outside `result` so it never enters
    /// model-facing tool output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_progress: Option<ToolProgressPatch>,
}

/// Progress update published on the per-instance control subject.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "ProgressMessageWire", into = "ProgressMessageWire")]
pub struct ProgressMessage {
    pub call_id: String,
    pub chunk: ProgressChunk,
}

#[derive(Serialize, Deserialize)]
struct ProgressMessageWire {
    call_id: String,
    kind: ProgressKind,
    chunk: ProgressChunk,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ProgressKind {
    Progress,
}

impl From<ProgressMessage> for ProgressMessageWire {
    fn from(message: ProgressMessage) -> Self {
        Self {
            call_id: message.call_id,
            kind: ProgressKind::Progress,
            chunk: message.chunk,
        }
    }
}

impl From<ProgressMessageWire> for ProgressMessage {
    fn from(message: ProgressMessageWire) -> Self {
        Self {
            call_id: message.call_id,
            chunk: message.chunk,
        }
    }
}

/// Tool server metadata stored in KV for discovery and schema publication.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registration {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    #[serde(default)]
    pub config: String,
    pub server: String,
    pub tools: Vec<ToolSpec>,
    pub schema_version: u32,
    pub proto_version: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::de::DeserializeOwned;
    use serde_json::json;

    fn assert_round_trip<T>(value: T)
    where
        T: Serialize + DeserializeOwned + fmt::Debug + PartialEq,
    {
        let encoded = serde_json::to_vec(&value).expect("serialize wire type");
        let decoded: T = serde_json::from_slice(&encoded).expect("deserialize wire type");
        assert_eq!(decoded, value);
    }

    fn tool_spec() -> ToolSpec {
        ToolSpec {
            cancellation_guarantee: Default::default(),
            name: "time_now".to_string(),
            description: "Return the current time".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": { "timezone": { "type": "string" } }
            }),
            idempotent_hint: true,
            read_only_hint: true,
            timeout_secs: Some(120),
            meta: None,
        }
    }

    #[test]
    fn tool_spec_without_timeout_remains_backward_compatible() {
        let value = serde_json::json!({
            "name": "echo",
            "description": "Echo input",
            "input_schema": { "type": "object" },
            "idempotent_hint": true,
            "read_only_hint": true
        });
        let spec: ToolSpec = serde_json::from_value(value).expect("decode legacy tool spec");
        assert_eq!(spec.timeout_secs, None);
        assert_eq!(spec.meta, None);
    }

    #[test]
    fn template_builders_create_and_extend_meta() {
        let spec = tool_spec();
        assert_eq!(spec.meta, None);

        let spec = spec
            .with_call_template("🕐 time")
            .with_result_template("{{ result.content[0].text }}");

        let meta = spec.meta.as_ref().expect("builders create the meta map");
        assert_eq!(meta["call_template"], json!("🕐 time"));
        assert_eq!(
            meta["result_template"],
            json!("{{ result.content[0].text }}")
        );
    }

    #[test]
    fn zero_timeout_explicitly_disables_the_request_deadline() {
        let spec = tool_spec().without_request_timeout();

        assert_eq!(spec.timeout_secs, Some(0));
        assert_eq!(
            serde_json::to_value(spec).expect("encode tool spec")["timeout_secs"],
            json!(0)
        );
    }

    #[test]
    fn template_builders_preserve_unrelated_meta_keys() {
        let mut spec = tool_spec();
        spec.meta = json!({ "vendor": "harnx" }).as_object().cloned();

        let spec = spec.with_call_template("call");

        let meta = spec.meta.as_ref().expect("meta map survives");
        assert_eq!(meta["vendor"], json!("harnx"));
        assert_eq!(meta["call_template"], json!("call"));
    }

    #[test]
    fn tool_spec_meta_round_trips_through_serde() {
        let mut spec = tool_spec();
        spec.meta = json!({ "call_template": "Calling {{tool}}" })
            .as_object()
            .cloned();

        assert_round_trip(spec);
    }

    #[test]
    fn is_subagent_launcher_true_cases() {
        // Bare names
        assert!(is_subagent_launcher("session_new"));
        assert!(is_subagent_launcher("session_prompt"));

        // Agent-prefixed (single underscore)
        assert!(is_subagent_launcher("oracle_session_new"));
        assert!(is_subagent_launcher("oracle_session_prompt"));
        assert!(is_subagent_launcher("pantheon_session_new"));
        assert!(is_subagent_launcher("pantheon_session_prompt"));

        // Package-prefixed (double underscore per package_namespace.rs)
        assert!(is_subagent_launcher("pantheon__oracle_session_new"));
        assert!(is_subagent_launcher("pantheon__oracle_session_prompt"));
    }

    #[test]
    fn is_subagent_launcher_false_cases() {
        // Other sub-agent control tools (NOT launchers)
        assert!(!is_subagent_launcher("session_load"));
        assert!(!is_subagent_launcher("session_cancel"));

        // Ordinary tools
        assert!(!is_subagent_launcher("read_file"));
        assert!(!is_subagent_launcher("bash_exec"));
        assert!(!is_subagent_launcher("web_search"));

        // Edge cases
        assert!(!is_subagent_launcher(""));

        // False positives to reject: names containing but not ending correctly
        assert!(!is_subagent_launcher("session_newer")); // prefix, not suffix
        assert!(!is_subagent_launcher("my_session_prompt_extra")); // has suffix after
        assert!(!is_subagent_launcher("session_prompting")); // different suffix
    }

    #[test]
    fn cancel_acceptance_variants_tag_and_round_trip_through_serde() {
        for (acceptance, tag) in [
            (CancelAcceptance::Accepted, "accepted"),
            (CancelAcceptance::AlreadyFinished, "already_finished"),
            (
                CancelAcceptance::Rejected {
                    reason: "wrong generation".into(),
                },
                "rejected",
            ),
            (
                CancelAcceptance::Unknown {
                    reason: "lost acknowledgement".into(),
                },
                "unknown",
            ),
        ] {
            let wire = serde_json::to_value(&acceptance).unwrap();
            assert_eq!(wire["kind"], tag);
            assert_round_trip(acceptance);
        }
    }

    #[test]
    fn protocol_version_is_5() {
        assert_eq!(TOOL_PROTOCOL_VERSION, 5);
    }

    #[test]
    fn control_message_carries_session_and_call_ids() {
        let control =
            ControlMessage::cancel("srv".into(), "sess".into(), "call-1".into(), "c-1".into());
        let json = serde_json::to_value(&control).unwrap();
        assert_eq!(json["session_id"], "sess");
        assert_eq!(json["call_id"], "call-1");
        assert_eq!(json["protocol_version"], TOOL_PROTOCOL_VERSION);
        let ack = control.acknowledgement(CancelAcceptance::AlreadyFinished);
        assert_eq!(ack.session_id, "sess");
        assert!(matches!(ack.acceptance, CancelAcceptance::AlreadyFinished));
    }

    #[test]
    fn wire_types_round_trip_through_serde() {
        assert_round_trip(tool_spec());
        assert_round_trip(ToolRequest {
            replay: None,
            operation_id: "call-1".to_string(),
            call_id: "call-1".to_string(),
            tool: "time_now".to_string(),
            args: json!({ "timezone": "UTC" }),
            parent_session_id: Some("parent-session".to_string()),
            tool_call_id: None,
            capabilities: BTreeSet::new(),
        });
        assert_round_trip(ToolRequest {
            replay: Some(ReplayAttempt {
                attempt: 2,
                requested_by: "worker-b".to_string(),
            }),
            operation_id: "call-1".to_string(),
            call_id: "call-1".to_string(),
            tool: "time_now".to_string(),
            args: json!({ "timezone": "UTC" }),
            parent_session_id: None,
            tool_call_id: None,
            capabilities: BTreeSet::new(),
        });
        assert_round_trip(ToolReply {
            call_id: "call-1".to_string(),
            result: Ok(json!({ "time": "12:00:00" })),
            final_progress: None,
        });
        assert_round_trip(ToolReply {
            call_id: "call-2".to_string(),
            result: Err(ToolErrorPayload::Recoverable(
                "unknown timezone".to_string(),
            )),
            final_progress: Some(ToolProgressPatch {
                title: Some("Checking timezone".into()),
                ..Default::default()
            }),
        });
        assert_round_trip(ControlMessage {
            protocol_version: TOOL_PROTOCOL_VERSION,
            server: "test".into(),
            session_id: "sess".to_string(),
            operation_id: "call-1".to_string(),
            cancellation_id: "cancel-test".into(),
            call_id: "call-1".to_string(),
            kind: ControlKind::Cancel,
        });
        assert_round_trip(ProgressMessage {
            call_id: "call-1".to_string(),
            chunk: ProgressChunk::V1(ToolProgressPatch {
                title: Some("Working".into()),
                ..Default::default()
            }),
        });
        assert_round_trip(Registration {
            package: None,
            config: String::new(),
            server: "time".to_string(),
            tools: vec![tool_spec()],
            schema_version: 1,
            proto_version: 1,
        });
    }

    #[test]
    fn tool_reply_final_progress_is_optional_and_outside_result() {
        let legacy = json!({
            "call_id": "call-legacy",
            "result": { "Ok": { "content": "done" } }
        });
        let reply: ToolReply = serde_json::from_value(legacy).unwrap();
        assert!(reply.final_progress.is_none());

        let wire = serde_json::to_value(reply).unwrap();
        assert!(wire.get("final_progress").is_none());
        assert_eq!(wire["result"]["Ok"]["content"], "done");
    }

    #[test]
    fn registration_without_identity_fields_uses_defaults() {
        let registration: Registration = serde_json::from_value(json!({
            "server": "time",
            "tools": [],
            "schema_version": 1,
            "proto_version": 1
        }))
        .expect("legacy registration should deserialize");

        assert_eq!(registration.package, None);
        assert_eq!(registration.config, "");
    }

    #[test]
    fn error_payload_variants_round_trip_through_serde() {
        assert_round_trip(ToolErrorPayload::Recoverable("retry".to_string()));
        assert_round_trip(ToolErrorPayload::Fatal("stop".to_string()));
    }

    #[test]
    fn interrupted_error_payload_round_trips() {
        let payload = ToolErrorPayload::Interrupted(InterruptedCall {
            cancellation_id: Some("c-1".into()),
            reason: "user".into(),
        });
        let back: ToolErrorPayload =
            serde_json::from_str(&serde_json::to_string(&payload).unwrap()).unwrap();
        assert!(
            matches!(back, ToolErrorPayload::Interrupted(InterruptedCall { cancellation_id: Some(id), .. }) if id == "c-1")
        );
    }

    #[test]
    fn server_identity_token_preserves_package_boundary() {
        assert_eq!(
            server_identity_token(Some("coding"), "time", "time"),
            "coding__time__time"
        );
        assert_eq!(server_identity_token(None, "time", "time"), "__time__time");
    }

    #[test]
    fn control_subject_messages_are_tagged_by_kind() {
        let cancel = serde_json::to_value(ControlMessage {
            protocol_version: TOOL_PROTOCOL_VERSION,
            server: "test".into(),
            session_id: "sess".to_string(),
            operation_id: "call-1".to_string(),
            cancellation_id: "cancel-test".into(),
            call_id: "call-1".to_string(),
            kind: ControlKind::Cancel,
        })
        .expect("serialize cancel");
        let progress = serde_json::to_value(ProgressMessage {
            call_id: "call-1".to_string(),
            chunk: ProgressChunk::V1(ToolProgressPatch {
                title: Some("Working".into()),
                ..Default::default()
            }),
        })
        .expect("serialize progress");

        assert_eq!(cancel["kind"], "cancel");
        assert_eq!(progress["kind"], "progress");
    }

    #[test]
    fn protocol_header_names_are_stable() {
        assert_eq!(HDR_IDEMPOTENCY_KEY, "Idempotency-Key");
        assert_eq!(HDR_CALL_ID, "X-Harnx-Call-Id");
        assert_eq!(HDR_INSTANCE_ID, "X-Harnx-Instance-Id");
        assert_eq!(HDR_CONTENT_TYPE, "Content-Type");
    }

    #[test]
    fn subagent_session_tool_names_are_stable() {
        assert_eq!(SUBAGENT_SESSION_NEW_TOOL, "session_new");
        assert_eq!(SUBAGENT_SESSION_PROMPT_TOOL, "session_prompt");
        assert_eq!(SUBAGENT_SESSION_LOAD_TOOL, "session_load");
        assert_eq!(SUBAGENT_SESSION_CANCEL_TOOL, "session_cancel");
    }
}

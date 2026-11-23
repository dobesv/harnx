//! Shared toolset contract and transport-independent protocol types.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
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
mod partial_result;
pub use partial_result::*;
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
    /// and server-liveness detection instead. MCP tool listings carry this
    /// policy in `_meta` under [`TOOL_TIMEOUT_META_KEY`].
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

/// `_meta` key a tool server uses to advertise its request-timeout policy, in
/// seconds, on transports whose tool listings carry no timeout field (MCP tools
/// do not). `harnx_toolset_server` writes it from `ToolSpec.timeout_secs`, and
/// `harnx-mcp-bridge` copies it back so NATS callers honor the wrapped server's
/// policy instead of the caller's default backstop. Zero disables the
/// elapsed-time deadline, matching `ToolSpec.timeout_secs`.
pub const TOOL_TIMEOUT_META_KEY: &str = "timeout_secs";

/// Read the timeout policy a tool advertised under [`TOOL_TIMEOUT_META_KEY`].
///
/// Absent or malformed values return `None`, so a tool that advertises no
/// policy keeps the caller's default request backstop instead of silently
/// losing its deadline.
pub fn advertised_timeout_secs(meta: &serde_json::Map<String, serde_json::Value>) -> Option<u64> {
    meta.get(TOOL_TIMEOUT_META_KEY)
        .and_then(serde_json::Value::as_u64)
}

/// `_meta` key a tool sets to `true` when its waits end early, with their
/// normal still-running result, once the caller's session gets a new message
/// (a [`ControlKind::NewMessage`] control). The caller reads it to decide
/// which in-flight calls to signal.
pub const STOP_ON_NEW_MESSAGE_META_KEY: &str = "harnx:stop_on_new_message";

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

    /// Meta key for static tool kind declaration.
    /// Tools declare their kind via `spec.with_kind(ToolProgressKind::Read)`.
    /// The runtime decodes this and maps it to `ToolDeclaration.kind`.
    const KIND_META_KEY: &'static str = "harnx:kind";

    /// Attach a static kind declaration to this tool spec.
    ///
    /// The kind is stored in the `meta` map under `"harnx:kind"` and is
    /// decoded by the runtime when building `ToolDeclaration`.
    #[must_use]
    pub fn with_kind(mut self, kind: ToolProgressKind) -> Self {
        self.meta.get_or_insert_with(serde_json::Map::new).insert(
            Self::KIND_META_KEY.to_string(),
            Value::String(kind.to_string()),
        );
        self
    }

    /// Declare that this tool's waits end early when a new message arrives
    /// for the caller's session; see [`STOP_ON_NEW_MESSAGE_META_KEY`].
    #[must_use]
    pub fn with_stop_on_new_message(mut self) -> Self {
        self.meta
            .get_or_insert_with(serde_json::Map::new)
            .insert(STOP_ON_NEW_MESSAGE_META_KEY.to_string(), Value::Bool(true));
        self
    }

    /// Whether this tool declared [`ToolSpec::with_stop_on_new_message`].
    pub fn stops_on_new_message(&self) -> bool {
        self.meta
            .as_ref()
            .and_then(|meta| meta.get(STOP_ON_NEW_MESSAGE_META_KEY))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    /// `_meta` key a tool sets to `true` when it may suspend a call by
    /// returning `input_required` (see [`ToolInputRequired`]). The runtime
    /// reads it to open the durable interaction path for rounds containing
    /// such a call even when no middleware is configured; a challenge from
    /// an undeclared tool degrades to an error result there.
    pub const INPUT_REQUIRED_META_KEY: &'static str = "harnx:input_required";

    /// Declare that this tool may return an input-required challenge; see
    /// [`ToolSpec::INPUT_REQUIRED_META_KEY`].
    #[must_use]
    pub fn with_input_required(mut self) -> Self {
        self.meta
            .get_or_insert_with(serde_json::Map::new)
            .insert(Self::INPUT_REQUIRED_META_KEY.to_string(), Value::Bool(true));
        self
    }

    /// Whether this tool declared [`ToolSpec::with_input_required`].
    pub fn requests_input(&self) -> bool {
        self.meta
            .as_ref()
            .and_then(|meta| meta.get(Self::INPUT_REQUIRED_META_KEY))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    /// Retrieve the static kind declaration from this tool spec.
    ///
    /// Returns `None` if no kind was declared or if the value is malformed.
    pub fn kind(&self) -> Option<ToolProgressKind> {
        self.meta
            .as_ref()?
            .get(Self::KIND_META_KEY)
            .and_then(|v| v.as_str())
            .and_then(|s| serde_json::from_value(Value::String(s.to_string())).ok())
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

/// One tool challenge asking the user for input before the call can finish —
/// the wire mapping of MCP MRTR `input_required` (SEP-2322).
///
/// A tool that needs to challenge again after a response issues a new
/// request with its own key and state; the caller re-dispatches with only
/// the latest challenge's responses and state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInputRequired {
    /// The tool's own request key (the MRTR `inputRequests` key). Distinct
    /// challenges need distinct keys: an exact repeat of an answered
    /// (key, kind, payload) gets its previous answer back.
    pub key: String,
    /// Interaction kind: `confirm`, `a2ui` or `url`.
    pub kind: String,
    /// The question text for `confirm` and `url` challenges.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The A2UI envelope for `a2ui` challenges (validated before suspending).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface: Option<Value>,
    /// The link URL for `url` challenges. Only `https:` and loopback `http:`
    /// are valid ([`ToolInputRequired::validate_url`]); anything else makes
    /// the challenge invalid, which degrades to a recoverable error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default)]
    pub summary: String,
    /// Opaque tool state resent beside the responses on the rerun, never
    /// inside `args`. Bounded by [`MAX_TOOL_REQUEST_STATE_BYTES`].
    #[serde(default)]
    pub request_state: Value,
}

impl ToolInputRequired {
    /// Frontends route on this kind with their confirm UI.
    pub const KIND_CONFIRM: &'static str = "confirm";
    /// Frontends route on this kind with their A2UI rendering.
    pub const KIND_A2UI: &'static str = "a2ui";
    /// Frontends route on this kind with their link UI (decision 6).
    pub const KIND_URL: &'static str = "url";

    /// Build a `confirm` challenge: a question with Approve/Deny semantics.
    pub fn confirm(
        key: impl Into<String>,
        message: impl Into<String>,
        request_state: Value,
    ) -> Self {
        Self {
            key: key.into(),
            kind: Self::KIND_CONFIRM.into(),
            message: Some(message.into()),
            surface: None,
            url: None,
            summary: String::new(),
            request_state,
        }
    }

    /// Build an `a2ui` challenge from a full A2UI envelope.
    pub fn a2ui(
        key: impl Into<String>,
        surface: Value,
        summary: impl Into<String>,
        request_state: Value,
    ) -> Self {
        Self {
            key: key.into(),
            kind: Self::KIND_A2UI.into(),
            message: None,
            surface: Some(surface),
            url: None,
            summary: summary.into(),
            request_state,
        }
    }

    /// Build a `url` challenge: a link the user opens out of band, answered
    /// with `continue` or `cancel` and no data. The URL must pass
    /// [`ToolInputRequired::validate_url`].
    pub fn url(
        key: impl Into<String>,
        url: impl Into<String>,
        message: impl Into<String>,
        request_state: Value,
    ) -> Self {
        Self {
            key: key.into(),
            kind: Self::KIND_URL.into(),
            message: Some(message.into()),
            surface: None,
            url: Some(url.into()),
            summary: String::new(),
            request_state,
        }
    }

    /// Check the wire shape. The runtime re-validates before suspending; this
    /// keeps oversized or malformed challenges from leaving the tool server.
    pub fn validate(&self) -> Result<(), String> {
        self.validate_key()?;
        self.validate_kind_fields()?;
        self.validate_state_size()
    }

    fn validate_key(&self) -> Result<(), String> {
        if self.key.is_empty() {
            return Err("input_required key must not be empty".into());
        }
        if self.key.len() > MAX_TOOL_INPUT_KEY_BYTES {
            return Err(format!(
                "input_required key exceeds {MAX_TOOL_INPUT_KEY_BYTES} bytes"
            ));
        }
        Ok(())
    }

    fn validate_kind_fields(&self) -> Result<(), String> {
        match self.kind.as_str() {
            Self::KIND_CONFIRM => self.validate_confirm_fields(),
            Self::KIND_A2UI => self.validate_a2ui_fields(),
            Self::KIND_URL => self.validate_url_fields(),
            other => Err(format!("unsupported input_required kind '{other}'")),
        }
    }

    fn validate_confirm_fields(&self) -> Result<(), String> {
        if optional_field_is_blank(self.message.as_deref()) {
            return Err("confirm input_required needs a message".into());
        }
        Ok(())
    }

    fn validate_a2ui_fields(&self) -> Result<(), String> {
        if self.surface.as_ref().is_none_or(Value::is_null) {
            return Err("a2ui input_required needs a surface".into());
        }
        Ok(())
    }

    fn validate_url_fields(&self) -> Result<(), String> {
        self.require_url_field()?;
        self.require_url_message()?;
        self.validate_url_value()
    }

    fn require_url_field(&self) -> Result<(), String> {
        if optional_field_is_blank(self.url.as_deref()) {
            return Err("url input_required needs a url".into());
        }
        Ok(())
    }

    fn require_url_message(&self) -> Result<(), String> {
        if optional_field_is_blank(self.message.as_deref()) {
            return Err("url input_required needs a message".into());
        }
        Ok(())
    }

    fn validate_url_value(&self) -> Result<(), String> {
        match self.url.as_deref() {
            Some(url) => Self::validate_url(url),
            None => Ok(()),
        }
    }

    fn validate_state_size(&self) -> Result<(), String> {
        let state = serde_json::to_vec(&self.request_state).unwrap_or_default();
        if state.len() > MAX_TOOL_REQUEST_STATE_BYTES {
            return Err(format!(
                "input_required request_state exceeds {MAX_TOOL_REQUEST_STATE_BYTES} bytes"
            ));
        }
        Ok(())
    }

    /// URL policy for `url` challenges (decision 6): only `https:` and
    /// loopback `http:` are valid. Everything else makes the challenge
    /// invalid; the runtime maps that to a recoverable error, never to a
    /// suspended interaction. `harnx-core` delegates its operator-exception
    /// copy to this one, so the rule has a single home.
    pub fn validate_url(raw: &str) -> Result<(), String> {
        let parsed = url::Url::parse(raw).map_err(|_| format!("invalid url '{raw}'"))?;
        match parsed.scheme() {
            "https" => Ok(()),
            "http" => match parsed.host() {
                Some(url::Host::Domain(host)) if host.eq_ignore_ascii_case("localhost") => Ok(()),
                Some(url::Host::Ipv4(ip)) if ip.is_loopback() => Ok(()),
                Some(url::Host::Ipv6(ip)) if ip.is_loopback() => Ok(()),
                _ => Err(format!("url '{raw}' must be https or loopback http")),
            },
            scheme => Err(format!(
                "url scheme '{scheme}' must be https or loopback http"
            )),
        }
    }
}

/// A challenge field that is absent or empty text carries no value on the wire.
fn optional_field_is_blank(field: Option<&str>) -> bool {
    field.map(str::is_empty).unwrap_or(true)
}

/// Answers to a tool's input-required challenges, keyed by the challenge's
/// request key (the MRTR `inputResponses` map).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolInputResponse {
    pub action: String,
    #[serde(default)]
    pub data: serde_json::Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Serialized bound on a tool's opaque `request_state`, enforced when the
/// challenge is built and again before the runtime suspends the call.
pub const MAX_TOOL_REQUEST_STATE_BYTES: usize = 64 * 1024;

/// Maximum number of input-required challenge rounds one tool call may
/// drive before the runtime gives up with an error result, matching rmcp's
/// MRTR client default (`DEFAULT_MRTR_MAX_ROUNDS`, SEP-2322); harnx
/// toolset-server asserts the two stay equal. Applies to every
/// `input_required` tool, native or MCP-bridged, so a looping tool cannot
/// keep a call suspended indefinitely.
pub const MAX_TOOL_INPUT_ROUNDS: usize = 10;

/// Serialized bound on a challenge's request key.
pub const MAX_TOOL_INPUT_KEY_BYTES: usize = 256;

/// Compatibility `result` error carried next to `ToolReply::input_required`
/// so callers that do not understand the field see a clear failure instead
/// of a silent success. Newer callers must ignore `result` when
/// `input_required` is set.
pub const INPUT_REQUIRED_COMPAT_ERROR: &str =
    "tool requires user input before it can finish; this caller cannot ask for it";

/// Error returned directly by a [`Toolset`] implementation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolInvokeError {
    Recoverable(String),
    Fatal(String),
    Interrupted(InterruptedCall),
    /// The tool cannot finish without user input (MRTR `input_required`).
    /// Not a failure: the caller suspends the call and re-dispatches with the
    /// accepted responses in [`ToolInvocationContext::input_responses`],
    /// never inside the tool's arguments. Boxed to keep the error type
    /// small (clippy `result_large_err`).
    InputRequired(Box<ToolInputRequired>),
}

impl fmt::Display for ToolInvokeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Recoverable(message) | Self::Fatal(message) => message.fmt(f),
            Self::Interrupted(interrupted) => interrupted.fmt(f),
            Self::InputRequired(input) => {
                write!(
                    f,
                    "tool input required ({}, kind {})",
                    input.key, input.kind
                )
            }
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
    pub run_context: Option<AutonomousRunContext>,
    /// Wire/RPC op id of this invocation (same as `ToolRequest::call_id`),
    /// NOT the parent transcript's tool-call id: a retried or replayed call
    /// mints a new one. Correlating a child row with the parent's transcript
    /// call needs the transport-injected `__harnx_tool_call_id` hidden
    /// argument instead.
    pub call_id: String,
    pub invoking_session_id: Option<String>,
    /// The caller's session identity (agent + local session id) for cid: URL construction.
    /// Set when the transport provides parent_agent and parent_local_session_id.
    pub invoking_session: Option<SessionRef>,
    pub capabilities: BTreeSet<String>,
    /// Handle for a checkpoint recorded on a prior attempt, if any. Set on
    /// replay so the tool can resume instead of restarting from scratch.
    pub checkpoint: Option<Value>,
    /// Where to record this call's checkpoint handle, so [`Toolset::cancel`]
    /// can act on it after the original invocation is gone. Absent when the
    /// transport does not support checkpointing.
    pub checkpoint_store: Option<Arc<dyn CheckpointStore>>,
    /// Where to record this call's partial result, which the runtime returns
    /// if the call does not succeed. Absent when the transport cannot keep
    /// one; [`ToolInvocationContext::record_partial_result`] then does nothing.
    pub partial_result_store: Option<Arc<dyn PartialResultStore>>,
    /// Call-bound live progress sink. Defaults to a no-op for transports and
    /// callers that do not negotiate progress support.
    pub progress: ToolProgressHandle,
    /// Fires when a new message arrives for the caller's session. A tool that
    /// opted in with [`ToolSpec::with_stop_on_new_message`] ends its wait and
    /// returns its normal still-running result; the call is not cancelled.
    /// Defaults to a token that never fires, for transports that cannot
    /// deliver the signal.
    pub new_message: CancellationToken,
    /// Answers to the tool's latest input-required challenge, keyed by the
    /// challenge's request key (MRTR `inputResponses`). Runtime-owned input
    /// delivered beside the arguments: never read from, or echoed into,
    /// model-authored tool arguments.
    pub input_responses: BTreeMap<String, ToolInputResponse>,
    /// The tool's opaque request state from its latest challenge (MRTR
    /// `requestState`), resent beside the responses. Null on a first call.
    pub request_state: Value,
    /// The caller's harnx user identity, decoded from
    /// [`ToolRequest::user_id`] by the transport. Trusted context: the worker
    /// fills it from the canonical session owner for model-originated calls
    /// and from the operator identity for operator calls, never from
    /// model-authored arguments. `None` for older callers and anonymous
    /// (ownerless) sessions.
    pub user_id: Option<String>,
}

/// Re-export SessionRef to allow harnx-toolset-server to pass it through
/// without creating a cycle via harnx-core dependency chain.
///
/// This is a minimal SessionRef defined here to avoid the circular dependency.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionRef {
    /// Agent name, or None for temp/inline sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// 6-char local session id (base64url, may start with `-`).
    pub session_id: String,
}

impl fmt::Debug for ToolInvocationContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolInvocationContext")
            .field("call_id", &self.call_id)
            .field("invoking_session_id", &self.invoking_session_id)
            .field("invoking_session", &self.invoking_session)
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
            .field(
                "partial_result_store",
                &if self.partial_result_store.is_some() {
                    "<set>"
                } else {
                    "<unset>"
                },
            )
            .field("progress", &self.progress)
            .field("new_message", &self.new_message.is_cancelled())
            .field("input_responses", &self.input_responses)
            .field("request_state", &self.request_state)
            .field("user_id", &self.user_id)
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

/// Immutable lineage carried by the runtime and saved with the journal request.
/// Snapshot is opaque to generic tool servers; the agent runtime validates its
/// RunLimitsRecord schema. Keeping it here avoids a toolset/core dependency cycle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutonomousRunContext {
    pub snapshot: Value,
    pub started_at_ms: u64,
}

/// How long before the caller's run deadline a wait stops, so the caller still
/// has time to read the result and answer before it is cancelled.
pub const FOREVER_DEADLINE_MARGIN: std::time::Duration = std::time::Duration::from_secs(60);

impl AutonomousRunContext {
    /// The caller's run deadline, when the snapshot carries one.
    pub fn deadline(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        let text = self.snapshot.get("deadline")?.as_str()?;
        chrono::DateTime::parse_from_rfc3339(text)
            .ok()
            .map(|deadline| deadline.with_timezone(&chrono::Utc))
    }

    /// How long a wait may run before it must stop for the caller's deadline:
    /// the time left minus `FOREVER_DEADLINE_MARGIN`, zero once that has
    /// passed. `None` when the caller has no deadline.
    pub fn wait_bound(&self) -> Option<std::time::Duration> {
        let left = (self.deadline()? - chrono::Utc::now())
            .to_std()
            .unwrap_or_default();
        Some(left.saturating_sub(FOREVER_DEADLINE_MARGIN))
    }
}

/// Request body for one tool invocation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRequest {
    /// Runtime-owned snapshot; never read from model-authored tool arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_context: Option<AutonomousRunContext>,
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
    /// Agent name of the invoking session (if available from a NATS caller).
    /// Combined with `parent_local_session_id`, gives the caller's identity
    /// for cid: URL construction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_agent: Option<String>,
    /// Local session id of the invoking session (if available from a NATS caller).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_local_session_id: Option<String>,
    /// The transcript's tool-call id (`ToolCall.id`) this invocation answers,
    /// when the caller has one. Wind-up and replay resolve a journal row by
    /// `(session, tool round, tool_call_id)`, never by `call_id`, because only
    /// the transcript id survives a worker restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Sequence of the transcript's `ToolCalls` entry that made this call,
    /// when it came from one. The journal keys the call's row by session,
    /// round, `tool_call_id` and `call_id`, and every party that reads or
    /// writes the row rebuilds that key from this request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_round: Option<u64>,
    /// Additive capabilities understood by the caller. An absent field means
    /// private result metadata must not be returned.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub capabilities: BTreeSet<String>,
    /// Answers to the tool's latest input-required challenge, keyed by the
    /// challenge's request key (MRTR `inputResponses`). Runtime-owned input,
    /// never read from model-authored tool arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_responses: Option<BTreeMap<String, ToolInputResponse>>,
    /// The tool's opaque request state from its latest challenge (MRTR
    /// `requestState`), resent beside the responses, never inside `args`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_state: Option<Value>,
    /// The caller's harnx user identity, for tool servers that act per user.
    /// Filled only by trusted callers: the worker stamps the canonical session
    /// owner for model-originated calls, or the operator identity for operator
    /// calls. It is transport-attested context like `parent_session_id`, never
    /// read from model-authored tool arguments, and absent for older callers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
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
/// Raw tool name for asynchronously dispatching a prompt to a sub-agent session.
pub const SUBAGENT_SESSION_SEND_TOOL: &str = "session_send";
/// Raw tool name for waiting on a previously dispatched sub-agent prompt.
pub const SUBAGENT_SESSION_WAIT_TOOL: &str = "session_wait";

/// Minimum elapsed time in milliseconds before showing a tool-call timer.
/// Tools running less than this duration show no elapsed-time display.
pub const TOOL_TIMER_MIN_ELAPSED_MS: u64 = 5_000;

/// Interval in milliseconds for CLI tool-call "still running" notices.
/// In append-only mode, a notice is printed each time this interval elapses.
pub const TOOL_TIMER_NOTICE_INTERVAL_MS: u64 = 10_000;

/// Returns `true` if the given tool name is a sub-agent launcher.
///
/// Launcher tools are `session_new`, `session_prompt` and `session_send`
/// (and their agent/package-prefixed forms). They are excluded from the
/// generic tool-call timer because they report their own progress: a
/// synchronous `session_prompt` streams live child progress, and a
/// dispatched `session_send` gets a background progress watcher whose row
/// already shows the child's own state. A dispatch receipt is a handle, not
/// a child completion the timer could meaningfully age. `session_wait` and
/// `session_load` are observation tools and are deliberately NOT launchers.
///
/// # Examples
/// ```
/// # use harnx_toolset::is_subagent_launcher;
/// assert!(is_subagent_launcher("session_new"));
/// assert!(is_subagent_launcher("session_prompt"));
/// assert!(is_subagent_launcher("session_send"));
/// assert!(is_subagent_launcher("oracle_session_send"));
/// assert!(is_subagent_launcher("pantheon__oracle_session_prompt"));
///
/// assert!(!is_subagent_launcher("session_wait"));
/// assert!(!is_subagent_launcher("session_load"));
/// assert!(!is_subagent_launcher("session_cancel"));
/// assert!(!is_subagent_launcher("read_file"));
/// assert!(!is_subagent_launcher(""));
/// ```
pub fn is_subagent_launcher(name: &str) -> bool {
    name == SUBAGENT_SESSION_NEW_TOOL
        || name == SUBAGENT_SESSION_PROMPT_TOOL
        || name == SUBAGENT_SESSION_SEND_TOOL
        || name.ends_with(&format!("_{}", SUBAGENT_SESSION_NEW_TOOL))
        || name.ends_with(&format!("_{}", SUBAGENT_SESSION_PROMPT_TOOL))
        || name.ends_with(&format!("_{}", SUBAGENT_SESSION_SEND_TOOL))
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
    /// The tool cannot finish without user input (MRTR `input_required`).
    /// When present, `result` carries the compatibility error
    /// [`INPUT_REQUIRED_COMPAT_ERROR`] for older callers and must be ignored
    /// by newer ones: the call suspends and is re-dispatched with the
    /// accepted responses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_required: Option<ToolInputRequired>,
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
    /// The user this registration is scoped to, when the registration holds
    /// one user's tools (per-user tool servers). Published under
    /// `{scope}.{identity_token}.u.{U}`; see [`registration_kv_key`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// True for an ownerless-only registration (`--single-user` servers),
    /// visible to sessions without a harnx user identity only. Published
    /// under `{scope}.{identity_token}.anon`; mutually exclusive with
    /// `user_id`.
    #[serde(default)]
    pub ownerless_only: bool,
    /// The credential connection this registration was derived from, for
    /// CAS fencing of refreshes and conditional deletes (a newer connection
    /// must not be overwritten or deleted by a stale writer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<String>,
}

impl Registration {
    /// Which sessions this registration is visible to (invariant (a) of the
    /// user-credentials plan: an owned session sees global registrations plus
    /// its owner's; an ownerless session sees global plus ownerless-only).
    pub fn audience(&self) -> RegistrationAudience {
        if let Some(user_id) = &self.user_id {
            RegistrationAudience::User(user_key_segment(user_id))
        } else if self.ownerless_only {
            RegistrationAudience::OwnerlessOnly
        } else {
            RegistrationAudience::Global
        }
    }
}

/// Which sessions one registration is visible to.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum RegistrationAudience {
    /// Every session under the scope.
    #[default]
    Global,
    /// Sessions whose owner's key segment equals this one (see
    /// [`user_key_segment`]).
    User(String),
    /// Sessions without an owner only (`--single-user` servers).
    OwnerlessOnly,
}

/// Key suffix segment separating a user-scoped registration key from its
/// server's global registration key. Older workers accept only keys exactly
/// equal to `{scope}.{identity_token}`, so `.u.{U}` and `.anon` keys are
/// invisible to them without a protocol-version bump.
pub const REGISTRATION_USER_SEGMENT: &str = "u";
/// Key suffix marking an ownerless-only registration.
pub const REGISTRATION_OWNERLESS_SEGMENT: &str = "anon";

/// Unpadded base64url encoding of a user id, safe in NATS KV keys and never
/// containing `.`, so a `{token}.u.{U}` key splits unambiguously.
pub fn user_key_segment(user_id: &str) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    URL_SAFE_NO_PAD.encode(user_id)
}

/// The KV key a registration publishes under: `{scope}.{identity_token}` for a
/// global registration, `{scope}.{identity_token}.u.{U}` for a user-scoped
/// one, `{scope}.{identity_token}.anon` for an ownerless-only one. All kinds
/// route to the same tool subject; only discovery visibility differs.
pub fn registration_kv_key(scope: &str, registration: &Registration) -> String {
    let identity_token = server_identity_token(
        registration.package.as_deref(),
        &registration.config,
        &registration.server,
    );
    match registration.audience() {
        RegistrationAudience::Global => format!("{scope}.{identity_token}"),
        RegistrationAudience::User(segment) => {
            format!("{scope}.{identity_token}.{REGISTRATION_USER_SEGMENT}.{segment}")
        }
        RegistrationAudience::OwnerlessOnly => {
            format!("{scope}.{identity_token}.{REGISTRATION_OWNERLESS_SEGMENT}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Split the part of a registry key after its `{scope}.` prefix into the
    /// server's identity token and the registration's audience.
    ///
    /// `None` for an empty suffix. A suffix without a recognized trailing
    /// `.u.{U}` or `.anon` segment classifies as global, which is exactly the key
    /// shape every pre-change worker accepted.
    fn split_registration_key_suffix(suffix: &str) -> Option<(String, RegistrationAudience)> {
        if suffix.is_empty() {
            return None;
        }
        if let Some(token) = suffix.strip_suffix(&format!(".{REGISTRATION_OWNERLESS_SEGMENT}")) {
            if !token.is_empty() {
                return Some((token.to_string(), RegistrationAudience::OwnerlessOnly));
            }
        }
        let user_separator = format!(".{REGISTRATION_USER_SEGMENT}.");
        if let Some((token, segment)) = suffix.rsplit_once(&user_separator) {
            if !token.is_empty() && !segment.is_empty() {
                return Some((
                    token.to_string(),
                    RegistrationAudience::User(segment.to_string()),
                ));
            }
        }
        Some((suffix.to_string(), RegistrationAudience::Global))
    }

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

    fn run_context(snapshot: Value) -> AutonomousRunContext {
        AutonomousRunContext {
            snapshot,
            started_at_ms: 0,
        }
    }

    #[test]
    fn run_context_deadline_parses_rfc3339_and_tolerates_absence() {
        let parsed = run_context(json!({"deadline": "2026-11-11T12:00:00Z"})).deadline();
        assert_eq!(
            parsed.map(|deadline| deadline.to_rfc3339()),
            Some("2026-11-11T12:00:00+00:00".to_string())
        );
        for snapshot in [json!({}), json!({"deadline": null}), json!({"deadline": 5})] {
            assert_eq!(run_context(snapshot).deadline(), None);
        }
    }

    #[test]
    fn run_context_wait_bound_deadline_leaves_the_margin() {
        use std::time::Duration;
        let bound = |secs: i64| {
            let deadline = chrono::Utc::now() + chrono::Duration::seconds(secs);
            run_context(json!({"deadline": deadline.to_rfc3339()})).wait_bound()
        };
        // (seconds until the deadline, expected bound range in seconds)
        let cases = [(100, 36..=40), (30, 0..=0), (-30, 0..=0)];
        for (secs, expected) in cases {
            let got = bound(secs).expect("bound").as_secs();
            assert!(expected.contains(&got), "{secs}s left gave {got}s");
        }
        assert_eq!(run_context(json!({})).wait_bound(), None::<Duration>);
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
            .with_call_template("time")
            .with_result_template("{{ result.content[0].text }}");

        let meta = spec.meta.as_ref().expect("builders create the meta map");
        assert_eq!(meta["call_template"], json!("time"));
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

    fn timeout_meta(value: &serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        value.as_object().expect("object literal").clone()
    }

    fn assert_parsed_timeout(value: serde_json::Value, expected: Option<u64>) {
        assert_eq!(
            advertised_timeout_secs(&timeout_meta(&value)),
            expected,
            "timeout metadata: {value}"
        );
    }

    #[test]
    fn advertised_timeout_secs_parses_policy_and_rejects_malformed_values() {
        let cases = [
            (json!({ "timeout_secs": 0 }), Some(0)),
            (json!({ "timeout_secs": 45 }), Some(45)),
            // Unadvertised policy must preserve the caller's request backstop.
            (json!({}), None),
            (json!({ "call_template": "x" }), None),
            (json!({ "timeout_secs": "none" }), None),
            (json!({ "timeout_secs": 1.5 }), None),
            (json!({ "timeout_secs": -1 }), None),
        ];
        for (value, expected) in cases {
            assert_parsed_timeout(value, expected);
        }
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
    fn kind_round_trips_through_meta() {
        use super::ToolProgressKind;

        let spec = tool_spec().with_kind(ToolProgressKind::Read);

        // Check kind is stored in meta
        let kind = spec.kind();
        assert_eq!(kind, Some(ToolProgressKind::Read));

        // Check it survives serialization
        let json = serde_json::to_value(&spec).expect("serialize spec");
        let meta = json.get("meta").expect("meta present");
        assert_eq!(meta["harnx:kind"], json!("read"));

        // Check it deserializes back
        let back: ToolSpec = serde_json::from_value(json).expect("deserialize spec");
        assert_eq!(back.kind(), Some(ToolProgressKind::Read));
    }

    #[test]
    fn kind_none_when_not_declared() {
        let spec = tool_spec();
        assert!(spec.kind().is_none());
    }

    #[test]
    fn kind_preserves_other_meta_keys() {
        use super::ToolProgressKind;

        let spec = tool_spec()
            .with_call_template("call")
            .with_kind(ToolProgressKind::Search);

        let meta = spec.meta.as_ref().expect("meta map created");
        assert_eq!(meta["call_template"], json!("call"));
        assert_eq!(meta["harnx:kind"], json!("search"));
        assert_eq!(spec.kind(), Some(ToolProgressKind::Search));
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
        assert!(is_subagent_launcher("session_send"));

        // Agent-prefixed (single underscore)
        assert!(is_subagent_launcher("oracle_session_new"));
        assert!(is_subagent_launcher("oracle_session_prompt"));
        assert!(is_subagent_launcher("oracle_session_send"));
        assert!(is_subagent_launcher("pantheon_session_new"));
        assert!(is_subagent_launcher("pantheon_session_prompt"));
        assert!(is_subagent_launcher("pantheon_session_send"));

        // Package-prefixed (double underscore per package_namespace.rs)
        assert!(is_subagent_launcher("pantheon__oracle_session_new"));
        assert!(is_subagent_launcher("pantheon__oracle_session_prompt"));
        assert!(is_subagent_launcher("pantheon__oracle_session_send"));
    }

    #[test]
    fn is_subagent_launcher_false_cases() {
        // Observation tools are NOT launchers: wait may repair activation, and
        // load is a pure read, but neither starts a child turn.
        assert!(!is_subagent_launcher("session_wait"));
        assert!(!is_subagent_launcher("session_load"));
        assert!(!is_subagent_launcher("session_cancel"));
        assert!(!is_subagent_launcher("oracle_session_wait"));
        assert!(!is_subagent_launcher("pantheon__oracle_session_wait"));

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
    fn new_message_control_is_tagged_new_message_and_round_trips() {
        let control = ControlMessage::new_message("srv".into(), "sess".into(), "call-1".into());
        assert_eq!(
            serde_json::to_value(&control).unwrap(),
            json!({
                "protocol_version": TOOL_PROTOCOL_VERSION,
                "server": "srv",
                "session_id": "sess",
                "operation_id": "call-1",
                "cancellation_id": "new-message:call-1",
                "call_id": "call-1",
                "kind": "new_message",
            })
        );
        assert_round_trip(control);
    }

    #[test]
    fn stop_on_new_message_round_trips_through_meta() {
        let spec = tool_spec().with_call_template("call");
        assert!(!spec.stops_on_new_message());
        let spec = spec.with_stop_on_new_message();
        assert!(spec.stops_on_new_message());
        let meta = spec.meta.as_ref().expect("meta map");
        assert_eq!(meta[STOP_ON_NEW_MESSAGE_META_KEY], json!(true));
        assert_eq!(meta["call_template"], json!("call"));
        let back: ToolSpec =
            serde_json::from_value(serde_json::to_value(&spec).unwrap()).expect("decode spec");
        assert!(back.stops_on_new_message());
    }

    #[test]
    fn invocation_context_new_message_never_fires_by_default() {
        assert_eq!(STOP_ON_NEW_MESSAGE_META_KEY, "harnx:stop_on_new_message");
        assert!(!ToolInvocationContext::default().new_message.is_cancelled());
    }

    #[test]
    fn input_required_reply_round_trips() {
        let reply = ToolReply {
            call_id: "call-9".to_string(),
            result: Err(ToolErrorPayload::Recoverable(
                INPUT_REQUIRED_COMPAT_ERROR.to_string(),
            )),
            final_progress: None,
            input_required: Some(ToolInputRequired::confirm(
                "ask-1",
                "Deploy to production?",
                json!({ "round": 1 }),
            )),
        };
        let back: ToolReply =
            serde_json::from_value(serde_json::to_value(&reply).unwrap()).unwrap();
        assert_eq!(back, reply);
        let input = back.input_required.expect("input_required round trips");
        input.validate().expect("constructed challenge validates");
    }

    #[test]
    fn old_callers_see_compatibility_error_and_new_callers_skip_result() {
        let reply = ToolReply {
            call_id: "call-9".to_string(),
            result: Err(ToolErrorPayload::Recoverable(
                INPUT_REQUIRED_COMPAT_ERROR.to_string(),
            )),
            final_progress: None,
            input_required: Some(ToolInputRequired::confirm("k", "Proceed?", Value::Null)),
        };
        // Newer callers ignore `result` entirely when a challenge is present.
        assert!(reply.input_required.is_some());
        assert!(reply.result.is_err());

        // An older caller decodes the same JSON and never sees the unknown
        // field, so it surfaces the compatibility error instead of a silent
        // success. Reply JSON without the field still decodes. `result`
        // is serde's externally tagged `{"Err": payload}` on the wire.
        let json = serde_json::to_value(&reply).unwrap();
        assert_eq!(json["result"]["Err"]["kind"], json!("recoverable"));
        let input_required = json.get("input_required").unwrap().clone();
        let legacy: serde_json::Value = serde_json::json!({
            "call_id": "call-9",
            "result": { "Err": { "kind": "recoverable", "message": INPUT_REQUIRED_COMPAT_ERROR } },
        });
        let legacy_reply: ToolReply = serde_json::from_value(legacy).unwrap();
        assert!(legacy_reply.input_required.is_none());
        assert!(legacy_reply.result.is_err());
        // Round-tripped challenges survive a decode by a caller that knows
        // the field but nothing else about the tool.
        let decoded: ToolInputRequired = serde_json::from_value(input_required).unwrap();
        assert_eq!(decoded.key, "k");
    }

    #[test]
    fn input_required_request_fields_round_trip() {
        let request = ToolRequest {
            run_context: None,
            replay: None,
            operation_id: "call-1".to_string(),
            call_id: "call-1".to_string(),
            tool: "deploy".to_string(),
            args: json!({ "target": "prod" }),
            parent_session_id: None,
            parent_agent: None,
            parent_local_session_id: None,
            tool_call_id: None,
            tool_round: None,
            capabilities: BTreeSet::new(),
            input_responses: Some(BTreeMap::from([(
                "ask-1".to_string(),
                ToolInputResponse {
                    action: "approve".to_string(),
                    data: serde_json::Map::new(),
                    note: Some("human response".to_string()),
                },
            )])),
            request_state: Some(json!({ "round": 2 })),
            user_id: None,
        };
        let back: ToolRequest =
            serde_json::from_value(serde_json::to_value(&request).unwrap()).unwrap();
        assert_eq!(back, request);
        // Older servers decode the same JSON without the new fields.
        let legacy: ToolRequest = serde_json::from_value(json!({
            "operation_id": "call-1",
            "call_id": "call-1",
            "tool": "deploy",
            "args": {},
        }))
        .unwrap();
        assert!(legacy.input_responses.is_none());
        assert!(legacy.request_state.is_none());
    }

    #[test]
    fn input_required_validate_rejects_bad_shapes() {
        let mut input = ToolInputRequired::confirm("", "Proceed?", Value::Null);
        assert!(input.validate().is_err());
        input = ToolInputRequired::confirm("k", "", Value::Null);
        assert!(input.validate().is_err());
        input = ToolInputRequired::a2ui("k", Value::Null, "s", Value::Null);
        assert!(input.validate().is_err());
        input = ToolInputRequired {
            kind: "mystery".to_string(),
            ..ToolInputRequired::confirm("k", "m", Value::Null)
        };
        assert!(input.validate().is_err());
        input = ToolInputRequired::confirm(
            "k",
            "m",
            json!({ "blob": "x".repeat(MAX_TOOL_REQUEST_STATE_BYTES) }),
        );
        assert!(input.validate().is_err());
        input =
            ToolInputRequired::confirm("x".repeat(MAX_TOOL_INPUT_KEY_BYTES + 1), "m", Value::Null);
        assert!(input.validate().is_err());
    }

    #[test]
    fn url_challenge_validates_scheme_and_loopback() {
        // Accepted: https anywhere, http only on loopback hosts.
        for url in [
            "https://example.com/connect/abc",
            "http://127.0.0.1:8931/connect/abc",
            "http://localhost:8931/connect/abc",
            "http://[::1]:8931/connect/abc",
            "http://127.8.4.2/connect/abc",
        ] {
            ToolInputRequired::validate_url(url).unwrap_or_else(|e| panic!("{url}: {e}"));
        }
        // Rejected: other schemes, non-loopback http, garbage.
        for url in [
            "http://example.com/connect/abc",
            "http://192.168.1.4/connect/abc",
            "ftp://example.com/x",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "not a url",
        ] {
            assert!(ToolInputRequired::validate_url(url).is_err(), "{url}");
        }
        // A well-formed challenge passes; missing fields and bad URLs fail.
        let input = ToolInputRequired::url(
            "k",
            "https://example.com/connect/abc",
            "Open the link to continue",
            json!({ "challenge_id": "abc" }),
        );
        input.validate().unwrap();
        assert!(
            ToolInputRequired::url("k", "http://example.com/x", "m", Value::Null)
                .validate()
                .is_err()
        );
        assert!(ToolInputRequired::url("k", "", "m", Value::Null)
            .validate()
            .is_err());
        assert!(
            ToolInputRequired::url("k", "https://example.com/x", "", Value::Null)
                .validate()
                .is_err()
        );
        assert_eq!(input.kind, ToolInputRequired::KIND_URL);
        assert_eq!(input.message.as_deref(), Some("Open the link to continue"));
        assert_eq!(
            input.url.as_deref(),
            Some("https://example.com/connect/abc")
        );
    }

    #[test]
    fn url_challenge_round_trips_and_legacy_json_decodes_without_url() {
        let input = ToolInputRequired::url(
            "k",
            "https://example.com/connect/abc",
            "Open the link",
            Value::Null,
        );
        let back: ToolInputRequired =
            serde_json::from_value(serde_json::to_value(&input).unwrap()).unwrap();
        assert_eq!(back, input);
        // The url field is skipped when absent; older JSON decodes unchanged.
        let legacy: ToolInputRequired = serde_json::from_value(json!({
            "key": "k",
            "kind": "confirm",
            "message": "Proceed?",
            "summary": "",
            "request_state": null,
        }))
        .unwrap();
        assert_eq!(legacy.message.as_deref(), Some("Proceed?"));
        assert!(legacy.url.is_none());
    }

    #[test]
    fn spec_input_required_round_trips_through_meta() {
        let spec = tool_spec();
        assert!(!spec.requests_input());
        let spec = spec.with_input_required();
        assert!(spec.requests_input());
        assert_eq!(ToolSpec::INPUT_REQUIRED_META_KEY, "harnx:input_required");
        let back: ToolSpec = serde_json::from_value(serde_json::to_value(&spec).unwrap()).unwrap();
        assert!(back.requests_input());
    }

    #[test]
    fn wire_types_round_trip_through_serde() {
        assert_round_trip(tool_spec());
        let request = ToolRequest {
            run_context: None,
            replay: None,
            operation_id: "call-1".to_string(),
            call_id: "call-1".to_string(),
            tool: "time_now".to_string(),
            args: json!({ "timezone": "UTC" }),
            parent_session_id: Some("parent-session".to_string()),
            parent_agent: None,
            parent_local_session_id: None,
            tool_call_id: Some("model-call".to_string()),
            tool_round: Some(7),
            capabilities: BTreeSet::new(),
            input_responses: None,
            request_state: None,
            user_id: None,
        };
        assert_round_trip(request.clone());
        assert_round_trip(ToolRequest {
            replay: Some(ReplayAttempt {
                attempt: 2,
                requested_by: "worker-b".to_string(),
            }),
            parent_session_id: None,
            tool_call_id: None,
            tool_round: None,
            ..request
        });
        assert_round_trip(ToolReply {
            call_id: "call-1".to_string(),
            result: Ok(json!({ "time": "12:00:00" })),
            final_progress: None,
            input_required: None,
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
            input_required: None,
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
            user_id: None,
            ownerless_only: false,
            connection_id: None,
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
    fn user_key_segment_is_unpadded_base64url_without_dots() {
        assert_eq!(user_key_segment("alice"), "YWxpY2U");
        assert_eq!(user_key_segment(""), "");
        assert!(!user_key_segment("a.b").contains('.'));
        // Round trip via the same encoding: two user ids never collide.
        assert_ne!(user_key_segment("a"), user_key_segment("YQ"));
    }

    fn registration_for(audience: RegistrationAudience) -> Registration {
        let mut registration = Registration {
            package: Some("pkg".to_string()),
            config: "cfg".to_string(),
            server: "jira".to_string(),
            tools: Vec::new(),
            schema_version: 1,
            proto_version: 1,
            user_id: None,
            ownerless_only: false,
            connection_id: None,
        };
        match audience {
            RegistrationAudience::Global => {}
            RegistrationAudience::User(_) => registration.user_id = Some("alice".to_string()),
            RegistrationAudience::OwnerlessOnly => registration.ownerless_only = true,
        }
        registration
    }

    #[test]
    fn registration_kv_keys_carry_the_audience_suffix() {
        let global =
            registration_kv_key("scope-1", &registration_for(RegistrationAudience::Global));
        assert_eq!(global, "scope-1.pkg__cfg__jira");
        let user = registration_kv_key(
            "scope-1",
            &registration_for(RegistrationAudience::User(String::new())),
        );
        assert_eq!(
            user,
            format!("scope-1.pkg__cfg__jira.u.{}", user_key_segment("alice"))
        );
        let ownerless = registration_kv_key(
            "scope-1",
            &registration_for(RegistrationAudience::OwnerlessOnly),
        );
        assert_eq!(ownerless, "scope-1.pkg__cfg__jira.anon");
    }

    #[test]
    fn split_registration_key_suffix_classifies_every_audience() {
        use RegistrationAudience::{Global, OwnerlessOnly, User};
        assert_eq!(
            split_registration_key_suffix("pkg__cfg__jira"),
            Some(("pkg__cfg__jira".to_string(), Global))
        );
        let segment = user_key_segment("alice");
        assert_eq!(
            split_registration_key_suffix(&format!("pkg__cfg__jira.u.{segment}")),
            Some(("pkg__cfg__jira".to_string(), User(segment.clone())))
        );
        assert_eq!(
            split_registration_key_suffix("pkg__cfg__jira.anon"),
            Some(("pkg__cfg__jira".to_string(), OwnerlessOnly))
        );
        assert_eq!(split_registration_key_suffix(""), None);
    }

    /// The pre-change worker accepted a registration only when its KV key was
    /// exactly `{scope}.{identity_token}`. `.u.{U}` and `.anon` keys must keep
    /// failing that exact-match rule (that is the no-protocol-bump
    /// compatibility story), while the audience-aware rule handles them.
    #[test]
    fn scoped_and_ownerless_keys_are_invisible_to_the_exact_match_rule() {
        let scope = "scope-1";
        let registrations = [
            registration_for(RegistrationAudience::Global),
            registration_for(RegistrationAudience::User(String::new())),
            registration_for(RegistrationAudience::OwnerlessOnly),
        ];
        for registration in &registrations {
            let key = registration_kv_key(scope, registration);
            let identity_token = server_identity_token(
                registration.package.as_deref(),
                &registration.config,
                &registration.server,
            );
            // The pre-change rule: one exact key shape per identity.
            let exact_match = key == format!("{scope}.{identity_token}");
            // The new rule classifies every key's audience.
            let (token, audience) =
                split_registration_key_suffix(&key[scope.len() + 1..]).expect("non-empty suffix");
            assert_eq!(token, identity_token);
            match registration.audience() {
                RegistrationAudience::Global => {
                    assert!(exact_match, "global keys stay exact-match compatible");
                    assert_eq!(audience, RegistrationAudience::Global);
                }
                RegistrationAudience::User(_) | RegistrationAudience::OwnerlessOnly => {
                    assert!(!exact_match, "scoped keys are ignored by the old rule");
                    assert_eq!(audience, registration.audience());
                }
            }
        }
    }

    #[test]
    fn registration_user_scoped_fields_default_on_legacy_payloads() {
        let registration: Registration = serde_json::from_value(json!({
            "server": "time",
            "tools": [],
            "schema_version": 1,
            "proto_version": 1
        }))
        .expect("legacy registration decodes");
        assert_eq!(registration.user_id, None);
        assert!(!registration.ownerless_only);
        assert_eq!(registration.connection_id, None);
        assert_eq!(registration.audience(), RegistrationAudience::Global);

        let scoped = serde_json::from_value::<Registration>(json!({
            "server": "jira",
            "tools": [],
            "schema_version": 1,
            "proto_version": 1,
            "user_id": "alice@example.com",
            "connection_id": "conn-7"
        }))
        .expect("user-scoped registration decodes");
        assert_eq!(
            scoped.audience(),
            RegistrationAudience::User(user_key_segment("alice@example.com"))
        );

        let encoded = serde_json::to_value(&scoped).unwrap();
        // `ownerless_only` is a defaulted plain bool, so it serializes
        // explicitly; a pre-change reader ignores the unknown field.
        assert_eq!(encoded["ownerless_only"], json!(false));
        assert_eq!(encoded["user_id"], json!("alice@example.com"));
        assert_eq!(encoded["connection_id"], json!("conn-7"));
    }

    #[test]
    fn tool_request_user_id_round_trips_and_defaults_to_absent() {
        let request: ToolRequest = serde_json::from_value(json!({
            "operation_id": "call-1",
            "call_id": "call-1",
            "tool": "deploy",
            "args": {}
        }))
        .expect("legacy request decodes");
        assert_eq!(request.user_id, None);

        let mut scoped = request.clone();
        scoped.user_id = Some("alice".to_string());
        let back: ToolRequest =
            serde_json::from_value(serde_json::to_value(&scoped).unwrap()).unwrap();
        assert_eq!(back, scoped);
        assert_eq!(back.user_id.as_deref(), Some("alice"));

        let encoded = serde_json::to_value(&scoped).unwrap();
        assert_eq!(encoded["user_id"], json!("alice"));
        let unscoped = serde_json::to_value(&request).unwrap();
        assert!(unscoped.get("user_id").is_none());
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

    #[test]
    fn async_subagent_session_tool_names_are_stable() {
        assert_eq!(SUBAGENT_SESSION_SEND_TOOL, "session_send");
        assert_eq!(SUBAGENT_SESSION_WAIT_TOOL, "session_wait");
    }
}

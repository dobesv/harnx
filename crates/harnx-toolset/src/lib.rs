//! Shared toolset contract and transport-independent protocol types.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fmt;
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

/// Schema and execution hints for one tool.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
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

/// Error returned directly by a [`Toolset`] implementation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolInvokeError {
    Recoverable(String),
    Fatal(String),
}

impl fmt::Display for ToolInvokeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Recoverable(message) | Self::Fatal(message) => message.fmt(f),
        }
    }
}

impl std::error::Error for ToolInvokeError {}

/// Collection of tools hosted by one tool server.
#[async_trait]
pub trait Toolset: Send + Sync {
    fn name(&self) -> &str;
    fn tools(&self) -> Vec<ToolSpec>;
    async fn invoke(
        &self,
        tool: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<Value, ToolInvokeError>;
}

/// Request body for one tool invocation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRequest {
    pub call_id: String,
    pub tool: String,
    pub args: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// Additive capabilities understood by the caller. An absent field means
    /// private result metadata must not be returned.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub capabilities: BTreeSet<String>,
}

/// Raw tool name for creating a sub-agent session.
pub const SUBAGENT_SESSION_NEW_TOOL: &str = "session_new";
/// Raw tool name for prompting a sub-agent session.
pub const SUBAGENT_SESSION_PROMPT_TOOL: &str = "session_prompt";
/// Raw tool name for loading a sub-agent session.
pub const SUBAGENT_SESSION_LOAD_TOOL: &str = "session_load";
/// Raw tool name for cancelling a sub-agent session.
pub const SUBAGENT_SESSION_CANCEL_TOOL: &str = "session_cancel";

/// Serializable error returned in a [`ToolReply`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "message", rename_all = "snake_case")]
pub enum ToolErrorPayload {
    Recoverable(String),
    Fatal(String),
}

/// Reply body for one tool invocation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolReply {
    pub call_id: String,
    pub result: Result<Value, ToolErrorPayload>,
}

/// Per-instance control message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlMessage {
    pub call_id: String,
    pub kind: ControlKind,
}

/// Kind discriminator for a [`ControlMessage`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlKind {
    Cancel,
}

/// Progress update published on the per-instance control subject.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "ProgressMessageWire", into = "ProgressMessageWire")]
pub struct ProgressMessage {
    pub call_id: String,
    pub chunk: Value,
}

#[derive(Serialize, Deserialize)]
struct ProgressMessageWire {
    call_id: String,
    kind: ProgressKind,
    chunk: Value,
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
    fn wire_types_round_trip_through_serde() {
        assert_round_trip(tool_spec());
        assert_round_trip(ToolRequest {
            call_id: "call-1".to_string(),
            tool: "time_now".to_string(),
            args: json!({ "timezone": "UTC" }),
            parent_session_id: Some("parent-session".to_string()),
            capabilities: BTreeSet::new(),
        });
        assert_round_trip(ToolReply {
            call_id: "call-1".to_string(),
            result: Ok(json!({ "time": "12:00:00" })),
        });
        assert_round_trip(ToolReply {
            call_id: "call-2".to_string(),
            result: Err(ToolErrorPayload::Recoverable(
                "unknown timezone".to_string(),
            )),
        });
        assert_round_trip(ControlMessage {
            call_id: "call-1".to_string(),
            kind: ControlKind::Cancel,
        });
        assert_round_trip(ProgressMessage {
            call_id: "call-1".to_string(),
            chunk: json!({ "completed": 1, "total": 2 }),
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
            call_id: "call-1".to_string(),
            kind: ControlKind::Cancel,
        })
        .expect("serialize cancel");
        let progress = serde_json::to_value(ProgressMessage {
            call_id: "call-1".to_string(),
            chunk: json!("working"),
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

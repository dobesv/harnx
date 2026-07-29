//! Shared toolset contract and transport-independent protocol types.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use tokio_util::sync::CancellationToken;

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
    /// Missing values use the client's default backstop for older registrations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
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
}

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
    }

    #[test]
    fn wire_types_round_trip_through_serde() {
        assert_round_trip(tool_spec());
        assert_round_trip(ToolRequest {
            call_id: "call-1".to_string(),
            tool: "time_now".to_string(),
            args: json!({ "timezone": "UTC" }),
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
            server: "time".to_string(),
            tools: vec![tool_spec()],
            schema_version: 1,
            proto_version: 1,
        });
    }

    #[test]
    fn error_payload_variants_round_trip_through_serde() {
        assert_round_trip(ToolErrorPayload::Recoverable("retry".to_string()));
        assert_round_trip(ToolErrorPayload::Fatal("stop".to_string()));
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
}

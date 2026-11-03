use crate::mcp::McpCallError;
use base64::{engine::general_purpose::STANDARD, Engine};
use harnx_core::safety::WRITE_MAX_BYTES;
use harnx_fs_tools::{AttachmentResolver, NatsAttachmentResolver, WriteAttachmentParams};
use harnx_toolset::ToolInvokeError;
use rmcp::{model::ErrorCode, service::ServiceError};
use serde_json::{Map, Value};
use std::error::Error;

const UNSUPPORTED: &str = "sandbox fs server does not support write_bytes; enable --sandbox-transport (HARNX_FS_SANDBOX_TRANSPORT=1) for harnx-fs-tools in the sandbox image";

pub(super) async fn resolve(
    args: Map<String, Value>,
    jetstream: &async_nats::jetstream::Context,
) -> Result<Map<String, Value>, ToolInvokeError> {
    let params: WriteAttachmentParams =
        serde_json::from_value(Value::Object(args)).map_err(|error| {
            super::recoverable(format!("invalid write_attachment arguments: {error}"))
        })?;
    let bytes = NatsAttachmentResolver::new(jetstream.clone())
        .resolve(&params.url, WRITE_MAX_BYTES)
        .await
        .map_err(super::recoverable)?;
    Ok(Map::from_iter([
        ("path".into(), Value::String(params.path)),
        ("data_base64".into(), Value::String(STANDARD.encode(bytes))),
    ]))
}

pub(super) fn remote_error(error: McpCallError) -> ToolInvokeError {
    // Both native and direct fs MCP adapters report unknown tools this way.
    // Require the protocol code and exact message, not incidental error text.
    let missing_tool = error
        .source()
        .and_then(|source| source.downcast_ref::<ServiceError>())
        .is_some_and(|source| {
            matches!(source, ServiceError::McpError(data)
                if data.code == ErrorCode::INVALID_PARAMS
                    && data.message == "unknown tool: write_bytes")
        });
    if missing_tool {
        super::recoverable(UNSUPPORTED)
    } else {
        super::remote_error(error)
    }
}

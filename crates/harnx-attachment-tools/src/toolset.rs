//! Attachment toolset implementation.

use anyhow::{Context, Result};
use async_nats::Client;
use async_trait::async_trait;
use harnx_blob_store::{create_or_open_attachments_bucket, touch_activity};
use harnx_nats_common::connect::NatsEndpoint;
use harnx_toolset::{ToolInvocation, ToolInvokeError, ToolProgressKind, ToolSpec, Toolset};

use crate::handlers::{
    attachment_create, attachment_read, AttachmentCreateParams, AttachmentReadParams,
};

const ATTACHMENT_READ_DESCRIPTION: &str = "Read an attachment by its cid: URL.

Returns image content as image blocks, text content as truncated text with
line numbers, and errors for non-displayable binary content.

Truncation parameters follow the same semantics as fs.read:
- head_lines: return only the first N lines
- tail_lines: return only the last N lines (combined with head_lines shows both ends)
- offset: start reading at this line (1-indexed)
- limit: maximum number of lines
- max_output_bytes: maximum output size in bytes
- grep: filter lines by regex pattern before truncation
";

const ATTACHMENT_CREATE_DESCRIPTION: &str = "Create a text attachment in the NATS object store.

Returns the cid: URL for the created attachment. Only text MIME types
are accepted (text/*, application/json, application/xml, application/yaml).

Requires caller session identity; returns an error when invoked via
MCP stdio/HTTP bridges that do not provide session context.
";

/// Attachment toolset for reading and creating NATS-backed blobs.
#[derive(Clone)]
pub struct AttachmentToolset {
    nats_url: Option<String>,
}

impl AttachmentToolset {
    /// Create a new attachment toolset.
    pub fn new() -> Self {
        Self { nats_url: None }
    }

    /// Create with a custom NATS URL (for testing).
    pub fn with_nats_url(url: impl Into<String>) -> Self {
        Self {
            nats_url: Some(url.into()),
        }
    }
}

impl Default for AttachmentToolset {
    fn default() -> Self {
        Self::new()
    }
}

async fn connect_nats(custom_url: Option<&str>) -> Result<Client> {
    match custom_url {
        Some(url) => async_nats::connect(url)
            .await
            .context("connect to NATS server"),
        None => NatsEndpoint::from_env()?.connect().await,
    }
}

#[async_trait]
impl Toolset for AttachmentToolset {
    fn name(&self) -> &str {
        "attachments"
    }

    fn default_mcp_http_port(&self) -> u16 {
        3007
    }

    fn tools(&self) -> Vec<ToolSpec> {
        vec![
            ToolSpec {
                name: "attachment_read".to_string(),
                description: ATTACHMENT_READ_DESCRIPTION.to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "The cid: URL to read"
                        },
                        "offset": { "type": "integer", "minimum": 1,
                            "description": "Start reading at this line number (1-indexed)" },
                        "limit": { "type": "integer",
                            "description": "Maximum number of lines to return from offset" },
                        "head_lines": {
                            "type": "integer",
                            "description": "Return only the first N lines"
                        },
                        "tail_lines": {
                            "type": "integer",
                            "description": "Return only the last N lines"
                        },
                        "max_output_bytes": {
                            "type": "integer",
                            "description": "Maximum output bytes"
                        },
                        "grep": {
                            "type": "string",
                            "description": "Filter lines by regex pattern"
                        }
                    },
                    "required": ["url"]
                }),
                cancellation_guarantee: harnx_toolset::CancellationGuarantee::HardOnDrop,
                idempotent_hint: true,
                read_only_hint: true,
                timeout_secs: None,
                meta: None,
            }
            .with_kind(ToolProgressKind::Read),
            ToolSpec {
                name: "attachment_create".to_string(),
                description: ATTACHMENT_CREATE_DESCRIPTION.to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "content": {
                            "type": "string",
                            "description": "The text content to store"
                        },
                        "mime_type": {
                            "type": "string",
                            "description": "MIME type (text/*, application/json, etc.)"
                        }
                    },
                    "required": ["content", "mime_type"]
                }),
                cancellation_guarantee: harnx_toolset::CancellationGuarantee::HardOnDrop,
                idempotent_hint: false,
                read_only_hint: false,
                timeout_secs: None,
                meta: None,
            }
            .with_kind(ToolProgressKind::Edit),
        ]
    }

    async fn invoke(
        &self,
        tool: &str,
        args: serde_json::Value,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<serde_json::Value, ToolInvokeError> {
        self.invoke_with_context(ToolInvocation {
            tool: tool.to_string(),
            args,
            context: harnx_toolset::ToolInvocationContext::default(),
            cancel,
        })
        .await
    }

    async fn invoke_with_context(
        &self,
        invocation: ToolInvocation,
    ) -> Result<serde_json::Value, ToolInvokeError> {
        // Get NATS client
        let client = connect_nats(self.nats_url.as_deref())
            .await
            .map_err(|e| ToolInvokeError::Recoverable(e.to_string()))?;

        // Get JetStream context
        let jetstream = async_nats::jetstream::new(client);

        // Get or create the attachments bucket
        let store = create_or_open_attachments_bucket(&jetstream, 1)
            .await
            .map_err(|e| ToolInvokeError::Recoverable(e.to_string()))?;

        invoke_with_context_tool(invocation, &store, &jetstream).await
    }
}

async fn touch_for_url(js: &async_nats::jetstream::Context, url_str: &str) {
    if let Ok(url) = harnx_core::cid_url::CidUrl::parse(url_str) {
        let _ = touch_activity(js, &url.owner()).await;
    }
}

async fn touch_for_session(
    js: &async_nats::jetstream::Context,
    session: Option<&harnx_toolset::SessionRef>,
) {
    if let Some(session) = session {
        let owner = harnx_core::session_identity::session_key(
            session.agent.as_deref(),
            &session.session_id,
        );
        let _ = touch_activity(js, &owner).await;
    }
}

async fn invoke_with_context_tool(
    invocation: ToolInvocation,
    store: &async_nats::jetstream::object_store::ObjectStore,
    jetstream: &async_nats::jetstream::Context,
) -> Result<serde_json::Value, ToolInvokeError> {
    match invocation.tool.as_str() {
        "attachment_read" => {
            let params: AttachmentReadParams = serde_json::from_value(invocation.args)
                .map_err(|e| ToolInvokeError::Recoverable(format!("invalid parameters: {}", e)))?;
            let url_str = params.url.clone();
            let result = attachment_read(store, params)
                .await
                .map_err(|e| ToolInvokeError::Recoverable(e.to_string()))?;
            touch_for_url(jetstream, &url_str).await;
            serde_json::to_value(result).map_err(|e| ToolInvokeError::Recoverable(e.to_string()))
        }
        "attachment_create" => {
            let params: AttachmentCreateParams = serde_json::from_value(invocation.args)
                .map_err(|e| ToolInvokeError::Recoverable(format!("invalid parameters: {}", e)))?;
            let caller_session = invocation.context.invoking_session.as_ref();
            let result = attachment_create(store, caller_session, params)
                .await
                .map_err(|e| ToolInvokeError::Recoverable(e.to_string()))?;
            touch_for_session(jetstream, caller_session).await;
            serde_json::to_value(result).map_err(|e| ToolInvokeError::Recoverable(e.to_string()))
        }
        _ => Err(ToolInvokeError::Recoverable(format!(
            "unknown attachment tool: {}",
            invocation.tool
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_tool_specs() {
        let toolset = AttachmentToolset::new();
        assert_eq!(toolset.name(), "attachments");
        assert_eq!(toolset.default_mcp_http_port(), 3007);

        let tools = toolset.tools();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "attachment_read");
        assert!(tools[0].input_schema["properties"].get("offset").is_some());
        assert!(tools[0].input_schema["properties"].get("limit").is_some());
        assert!(tools[0].idempotent_hint);
        assert!(tools[0].read_only_hint);
        assert_eq!(tools[1].name, "attachment_create");
    }
}

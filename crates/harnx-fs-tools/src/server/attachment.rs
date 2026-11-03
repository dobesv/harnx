use super::{internal_error, invalid_params, validate_write_path, FsServer, WriteAttachmentParams};
use crate::AttachmentResolver;
use harnx_core::{cid_url::CidUrl, safety::WRITE_MAX_BYTES};
use rmcp::model::{CallToolResult, ErrorData};
use std::sync::Arc;

impl FsServer {
    /// Configure attachment access for both native and direct MCP handlers.
    pub fn with_attachment_resolver(mut self, resolver: Arc<dyn AttachmentResolver>) -> Self {
        self.attachment_resolver = Some(resolver);
        self
    }

    /// None for servers without attachment access, including default MCP servers.
    pub fn attachment_resolver(&self) -> Option<&Arc<dyn AttachmentResolver>> {
        self.attachment_resolver.as_ref()
    }

    pub(crate) async fn write_attachment_impl(
        &self,
        params: WriteAttachmentParams,
    ) -> Result<CallToolResult, ErrorData> {
        validate_write_path(&params.path, &self.allowlist).map_err(invalid_params)?;
        CidUrl::parse(&params.url)
            .map_err(|error| invalid_params(format!("Invalid attachment URL: {error:#}")))?;
        let resolver = self.attachment_resolver().ok_or_else(|| {
            internal_error(
                "Attachment resolution unavailable: no resolver configured for this filesystem server.",
            )
        })?;
        // Resolve before locking or creating directories so source failures leave disk untouched.
        let bytes = resolver
            .resolve(&params.url, WRITE_MAX_BYTES)
            .await
            .map_err(|error| internal_error(error.to_string()))?;
        self.write_bytes_impl(&params.path, &bytes).await
    }
}

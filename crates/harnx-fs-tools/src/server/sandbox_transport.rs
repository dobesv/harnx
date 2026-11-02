use super::{invalid_params, validate_write_path, FsServer, WriteBytesParams};
use base64::{engine::general_purpose::STANDARD, Engine};
use harnx_core::safety::WRITE_MAX_BYTES;
use rmcp::model::{CallToolResult, ErrorData};
use serde_json::{Map, Value};

pub(crate) const WRITE_MAX_BASE64_BYTES: usize = WRITE_MAX_BYTES.div_ceil(3) * 4;
/// Room for a maximum-size base64 file plus the MCP envelope and path.
pub const SANDBOX_HTTP_MAX_REQUEST_BODY_BYTES: usize = WRITE_MAX_BASE64_BYTES + 1024 * 1024;
pub(crate) const WRITE_BYTES_DESCRIPTION: &str =
    "Sandbox transport only: decode standard base64 and create or overwrite a file with the exact bytes. Maximum decoded size: 100 MiB.";

/// Observations and progress need locations, never file contents or base64 payloads.
pub(crate) fn location_args(args: &Value) -> Value {
    Value::Object(
        ["path", "repo_path"]
            .into_iter()
            .filter_map(|key| {
                args.get(key)
                    .and_then(Value::as_str)
                    .map(|path| (key.to_string(), Value::String(path.to_string())))
            })
            .collect::<Map<_, _>>(),
    )
}

impl FsServer {
    /// Enable the private byte transport used by the sandbox gateway.
    pub fn with_sandbox_transport(mut self) -> Self {
        self.sandbox_transport = true;
        self
    }

    pub fn sandbox_transport_enabled(&self) -> bool {
        self.sandbox_transport
    }

    pub(crate) async fn write_base64_impl(
        &self,
        params: WriteBytesParams,
    ) -> Result<CallToolResult, ErrorData> {
        let WriteBytesParams { path, data_base64 } = params;
        validate_write_path(&path, &self.allowlist).map_err(invalid_params)?;
        let bytes = decode_payload(&data_base64)?;
        drop(data_base64);
        self.write_bytes_impl(&path, &bytes).await
    }
}

fn decode_payload(encoded: &str) -> Result<Vec<u8>, ErrorData> {
    if encoded.len() > WRITE_MAX_BASE64_BYTES {
        return Err(invalid_params(format!(
            "Encoded content too large ({} bytes, max {WRITE_MAX_BASE64_BYTES} bytes)",
            encoded.len()
        )));
    }
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|error| invalid_params(format!("Invalid base64 data: {error}")))?;
    // Up to two extra decoded bytes can fit in the same encoded-length bound.
    if bytes.len() > WRITE_MAX_BYTES {
        return Err(invalid_params(format!(
            "Decoded content too large ({} bytes, max {WRITE_MAX_BYTES} bytes)",
            bytes.len()
        )));
    }
    Ok(bytes)
}

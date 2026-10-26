//! Per-result presentation limits. Stored originals are never truncated.

mod parts;
mod storage;

use async_nats::jetstream;
use base64::{engine::general_purpose::STANDARD, Engine};
use harnx_blob_store::media::MediaBlob;
use harnx_core::cid_url::SessionRef;
use harnx_toolset::ToolInvokeError;
use serde_json::{json, Value};

use parts::replace_part;
use storage::{data_url_cid, fatal, AttachmentUploads, Storage};

pub const DEFAULT_MAX_RESULT_BYTES: usize = 32_768;
/// Room for an explicit error, attachment reference and retrieval instructions.
pub const MIN_MAX_RESULT_BYTES: usize = 1024;

pub fn parse_max_result_bytes(raw: &str) -> Result<usize, String> {
    let value = raw
        .parse::<usize>()
        .map_err(|_| "expected a positive byte count".to_string())?;
    if value < MIN_MAX_RESULT_BYTES {
        return Err(format!(
            "must be at least {MIN_MAX_RESULT_BYTES} serialized bytes"
        ));
    }
    Ok(value)
}

/// Reuses the serving connection; callers without attested ownership cannot offload.
pub struct ResultLimits {
    max_bytes: usize,
    storage: Option<Storage>,
}

impl Default for ResultLimits {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_RESULT_BYTES,
            storage: None,
        }
    }
}

impl ResultLimits {
    pub fn new(max_bytes: usize) -> anyhow::Result<Self> {
        anyhow::ensure!(
            max_bytes >= MIN_MAX_RESULT_BYTES,
            "max-result-bytes must be at least {MIN_MAX_RESULT_BYTES}"
        );
        Ok(Self {
            max_bytes,
            storage: None,
        })
    }

    pub fn with_storage(mut self, jetstream: jetstream::Context, replicas: usize) -> Self {
        self.storage = Some(Storage::new(jetstream, replicas));
        self
    }

    pub async fn normalize(
        &self,
        original: Value,
        session: Option<&SessionRef>,
    ) -> Result<Value, ToolInvokeError> {
        let bytes = serde_json::to_vec(&original).map_err(fatal)?;
        if bytes.len() <= self.max_bytes {
            return Ok(original);
        }
        let (Some(session), Some(storage)) = (session, &self.storage) else {
            return Ok(error("MCP result exceeds max-result-bytes; original was not retained because trusted session attachment storage is unavailable. Tool side effects may already have occurred. Narrow the tool request before retrying."));
        };
        StoredResult {
            original: &original,
            bytes: &bytes,
            session,
            uploads: storage.uploads(session).await?,
        }
        .normalize(self.max_bytes)
        .await
    }
}

#[derive(Clone, Copy)]
enum AggregateReason {
    PreserveFields,
    SchemaError,
}

struct PartPlan {
    indices: Vec<usize>,
    budget: usize,
}

impl PartPlan {
    fn new(original: &Value, max_bytes: usize) -> Result<Self, AggregateReason> {
        let parts = original
            .get("content")
            .and_then(Value::as_array)
            .ok_or(AggregateReason::SchemaError)?;
        // Only parts that cannot fit alone in a minimal result are oversized.
        // Many individually fitting parts use the explicit aggregate fallback.
        let indices: Vec<usize> = parts
            .iter()
            .enumerate()
            .filter(|(_, part)| serialized_len(&json!({"content": [part]})) > max_bytes)
            .map(|(index, _)| index)
            .collect();
        if indices.is_empty() {
            return Err(AggregateReason::PreserveFields);
        }
        let mut skeleton = original.clone();
        for &index in &indices {
            skeleton["content"][index] = Value::Null;
        }
        let base_bytes = serialized_len(&skeleton).saturating_sub(4 * indices.len());
        let available = max_bytes
            .checked_sub(base_bytes)
            .ok_or(AggregateReason::PreserveFields)?;
        Ok(Self {
            budget: available / indices.len(),
            indices,
        })
    }
}

/// Original JSON and upload capability stay together throughout fallback handling.
struct StoredResult<'a> {
    original: &'a Value,
    bytes: &'a [u8],
    session: &'a SessionRef,
    uploads: AttachmentUploads<'a>,
}

impl StoredResult<'_> {
    async fn normalize(&self, max_bytes: usize) -> Result<Value, ToolInvokeError> {
        let plan = match PartPlan::new(self.original, max_bytes) {
            Ok(plan) => plan,
            Err(reason) => return self.aggregate(max_bytes, reason).await,
        };
        let mut result = self.original.clone();
        for index in plan.indices {
            let Some(replacement) = replace_part(
                &result["content"][index],
                plan.budget,
                self.session,
                &self.uploads,
            )
            .await?
            else {
                return self
                    .aggregate(max_bytes, AggregateReason::SchemaError)
                    .await;
            };
            result["content"][index] = replacement;
        }
        // Escaping, metadata, commas and all envelope fields count toward the cap.
        if serialized_len(&result) <= max_bytes {
            Ok(result)
        } else {
            self.aggregate(max_bytes, AggregateReason::SchemaError)
                .await
        }
    }

    async fn aggregate(
        &self,
        max_bytes: usize,
        reason: AggregateReason,
    ) -> Result<Value, ToolInvokeError> {
        let url = data_url_cid(
            self.session,
            "application/json",
            &STANDARD.encode(self.bytes),
        );
        self.uploads
            .put(MediaBlob {
                url: &url,
                bytes: self.bytes,
                mime_type: "application/json",
            })
            .await?;
        let marker = format!("MCP result exceeds max-result-bytes. Full original result (application/json): {url}. Read with attachment_read using byte_offset/byte_limit, or narrow the tool request.");
        let mut aggregate = self.original.clone();
        aggregate["content"] = json!([{"type": "text", "text": marker}]);
        if matches!(reason, AggregateReason::PreserveFields)
            && serialized_len(&aggregate) <= max_bytes
        {
            return Ok(aggregate);
        }
        // Never substitute arbitrary objects into structuredContent or _meta.
        let result = error(&format!("{marker} Original structuredContent, metadata or content cannot be represented within the limit; this is an explicit size error, not a replacement output schema. Tool side effects may already have occurred."));
        if serialized_len(&result) <= max_bytes {
            Ok(result)
        } else {
            // Session identities themselves can make a CID too long for the cap.
            Ok(error("MCP result exceeds max-result-bytes. Original retained in session attachment storage, but its reference cannot fit within this limit. Increase max-result-bytes. Tool side effects may already have occurred."))
        }
    }
}

fn serialized_len(value: &Value) -> usize {
    // Values received from serde_json cannot fail serialization.
    serde_json::to_vec(value)
        .expect("serialize JSON Value")
        .len()
}

fn error(message: &str) -> Value {
    json!({"content": [{"type": "text", "text": message}], "isError": true})
}

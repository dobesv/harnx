//! Per-result presentation limits. Stored originals are never truncated.

use async_nats::jetstream::{self, object_store::ObjectStore};
use base64::Engine;
use harnx_core::{
    attachments::cid_for_data_url_with_session,
    cid_url::{CidUrl, SessionRef},
    safety::{truncate_output, TruncateOpts},
};
use harnx_toolset::ToolInvokeError;
use serde_json::{json, Value};
use tokio::sync::OnceCell;

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

struct Storage {
    jetstream: jetstream::Context,
    replicas: usize,
    bucket: OnceCell<ObjectStore>,
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
        self.storage = Some(Storage {
            jetstream,
            replicas,
            bucket: OnceCell::new(),
        });
        self
    }

    pub async fn normalize(
        &self,
        original: Value,
        session: Option<&SessionRef>,
    ) -> Result<Value, ToolInvokeError> {
        let original_bytes = serde_json::to_vec(&original).map_err(fatal)?;
        if original_bytes.len() <= self.max_bytes {
            return Ok(original);
        }
        let (Some(session), Some(storage)) = (session, &self.storage) else {
            return Ok(error("MCP result exceeds max-result-bytes; original was not retained because trusted session attachment storage is unavailable. Tool side effects may already have occurred. Narrow the tool request before retrying."));
        };
        let store = storage
            .bucket
            .get_or_try_init(|| async {
                harnx_blob_store::create_or_open_attachments_bucket(
                    &storage.jetstream,
                    storage.replicas,
                )
                .await
            })
            .await
            .map_err(fatal)?;

        let _ = harnx_blob_store::touch_activity(&storage.jetstream, &session.owner()).await;
        let mut result = original.clone();
        let Some(parts) = result.get_mut("content").and_then(Value::as_array_mut) else {
            return self
                .aggregate(&original, &original_bytes, session, store, true)
                .await;
        };
        // A part is oversized only if it cannot fit on its own in a minimal result.
        // Many individually fitting parts use the explicit aggregate fallback instead.
        let oversized: Vec<usize> = parts
            .iter()
            .enumerate()
            .filter(|(_, part)| serialized_len(&json!({"content": [part]})) > self.max_bytes)
            .map(|(index, _)| index)
            .collect();
        if oversized.is_empty() {
            return self
                .aggregate(&original, &original_bytes, session, store, false)
                .await;
        }
        let mut skeleton = original.clone();
        for &index in &oversized {
            skeleton["content"][index] = Value::Null;
        }
        let base_bytes = serialized_len(&skeleton).saturating_sub(4 * oversized.len());
        let Some(available) = self.max_bytes.checked_sub(base_bytes) else {
            return self
                .aggregate(&original, &original_bytes, session, store, false)
                .await;
        };
        let part_budget = available / oversized.len();
        for index in oversized {
            let Some(replacement) =
                replace_part(&parts[index], part_budget, session, store).await?
            else {
                return self
                    .aggregate(&original, &original_bytes, session, store, true)
                    .await;
            };
            parts[index] = replacement;
        }
        // Escaping, metadata, commas and all envelope fields count toward the cap.
        if serialized_len(&result) <= self.max_bytes {
            Ok(result)
        } else {
            self.aggregate(&original, &original_bytes, session, store, true)
                .await
        }
    }

    async fn aggregate(
        &self,
        original: &Value,
        bytes: &[u8],
        session: &SessionRef,
        store: &ObjectStore,
        schema_error: bool,
    ) -> Result<Value, ToolInvokeError> {
        let url = data_url_cid(
            session,
            "application/json",
            &base64::engine::general_purpose::STANDARD.encode(bytes),
        );
        harnx_blob_store::put_media(store, &url, bytes, "application/json")
            .await
            .map_err(fatal)?;
        let marker = format!("MCP result exceeds max-result-bytes. Full original result (application/json): {url}. Read with attachment_read using byte_offset/byte_limit, or narrow the tool request.");
        let mut aggregate = original.clone();
        aggregate["content"] = json!([{"type": "text", "text": marker}]);
        if !schema_error && serialized_len(&aggregate) <= self.max_bytes {
            return Ok(aggregate);
        }
        // Never substitute arbitrary objects into structuredContent or _meta.
        let result = error(&format!("{marker} Original structuredContent, metadata or content cannot be represented within the limit; this is an explicit size error, not a replacement output schema. Tool side effects may already have occurred."));
        if serialized_len(&result) <= self.max_bytes {
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

fn fatal(error: impl std::fmt::Display) -> ToolInvokeError {
    ToolInvokeError::Fatal(format!("MCP result attachment storage failed: {error:#}"))
}

async fn replace_part(
    part: &Value,
    budget: usize,
    session: &SessionRef,
    store: &ObjectStore,
) -> Result<Option<Value>, ToolInvokeError> {
    let kind = part["type"].as_str().unwrap_or_default();
    let (payload, text_field) = match kind {
        "text" => (part, "text"),
        "resource" => (&part["resource"], "text"),
        "image" | "audio" => (part, ""),
        _ => return Ok(None),
    };
    let text = payload.get(text_field).and_then(Value::as_str);
    // A resource_link has only one _meta slot. Flattening nested resource metadata
    // into it would change semantics (and can collide with part-level _meta).
    if kind == "resource" && text.is_none() && payload.get("_meta").is_some() {
        return Ok(None);
    }
    let mime = payload
        .get("mimeType")
        .and_then(Value::as_str)
        .unwrap_or(if text.is_some() {
            "text/plain"
        } else {
            "application/octet-stream"
        });
    let (bytes, url) = if let Some(text) = text {
        (
            text.as_bytes().to_vec(),
            data_url_cid(
                session,
                mime,
                &base64::engine::general_purpose::STANDARD.encode(text),
            ),
        )
    } else {
        let Some(encoded) = payload
            .get(if kind == "resource" { "blob" } else { "data" })
            .and_then(Value::as_str)
        else {
            return Ok(None);
        };
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(encoded) else {
            // Keep malformed payloads recoverable as full JSON, never cut base64.
            return Ok(None);
        };
        // Images must use the same full-data-URI hash as worker attachment caches.
        let url = data_url_cid(session, mime, encoded);
        (bytes, url)
    };
    if let Some(text) = text {
        let marker = format!("\nOriginal ({mime}): {url}. Read with attachment_read using byte_offset/byte_limit, or narrow the tool request.");
        let mut minimal = part.clone();
        if kind == "resource" {
            minimal["resource"]["text"] = Value::String(marker.clone());
        } else {
            minimal["text"] = Value::String(marker.clone());
        }
        if serialized_len(&minimal) > budget {
            return Ok(None);
        }
        harnx_blob_store::put_media(store, &url, &bytes, mime)
            .await
            .map_err(fatal)?;
        let mut preview_budget = budget;
        while preview_budget > 0 {
            let preview = truncate_output(
                text,
                &TruncateOpts {
                    max_output_bytes: preview_budget,
                    ..Default::default()
                },
            );
            let mut replacement = minimal.clone();
            let target = if kind == "resource" {
                &mut replacement["resource"]
            } else {
                &mut replacement
            };
            target["text"] = Value::String(format!("{preview}{marker}"));
            if serialized_len(&replacement) <= budget {
                return Ok(Some(replacement));
            }
            preview_budget /= 2;
        }
        return Ok(Some(minimal));
    }
    let mut replacement = json!({"type": "resource_link", "uri": url.to_string(), "name": format!("{kind} attachment"), "mimeType": mime, "size": bytes.len()});
    if kind == "resource" {
        // The link's URI now points to durable bytes; keep the resource's source URI.
        if let Some(uri) = payload.get("uri").and_then(Value::as_str) {
            replacement["description"] = Value::String(format!("Original resource: {uri}"));
        }
    }
    for field in ["annotations", "_meta"] {
        if let Some(value) = part.get(field) {
            replacement[field] = value.clone();
        }
    }
    if serialized_len(&replacement) > budget {
        return Ok(None);
    }
    harnx_blob_store::put_media(store, &url, &bytes, mime)
        .await
        .map_err(fatal)?;
    Ok(Some(replacement))
}

fn data_url_cid(session: &SessionRef, mime: &str, encoded: &str) -> CidUrl {
    CidUrl::parse(&cid_for_data_url_with_session(
        session,
        &format!("data:{mime};base64,{encoded}"),
    ))
    .expect("canonical media CID")
}

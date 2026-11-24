//! Per-part replacements preserve bytes, MIME and representable part fields.

use base64::{engine::general_purpose::STANDARD, Engine};
use harnx_blob_store::media::MediaBlob;
use harnx_core::{
    cid_url::{CidUrl, SessionRef},
    safety::{truncate_output, TruncateOpts},
};
use harnx_toolset::ToolInvokeError;
use serde_json::{json, Value};

use super::{
    serialized_len,
    storage::{data_url_cid, AttachmentUploads},
};

struct Part<'a> {
    original: &'a Value,
    payload: &'a Value,
    kind: &'a str,
}

impl<'a> Part<'a> {
    fn parse(original: &'a Value) -> Option<Self> {
        let kind = original["type"].as_str().unwrap_or_default();
        let payload = match kind {
            "text" | "image" | "audio" => original,
            "resource" => &original["resource"],
            _ => return None,
        };
        Some(Self {
            original,
            payload,
            kind,
        })
    }

    fn text(&self) -> Option<&'a str> {
        let field = match self.kind {
            "text" | "resource" => "text",
            _ => "",
        };
        self.payload.get(field).and_then(Value::as_str)
    }

    fn mime(&self) -> &'a str {
        let default = match self.text() {
            Some(_) => "text/plain",
            None => "application/octet-stream",
        };
        self.payload
            .get("mimeType")
            .and_then(Value::as_str)
            .unwrap_or(default)
    }

    fn binary(&self, session: &SessionRef) -> Option<(Vec<u8>, CidUrl)> {
        let field = match self.kind {
            "resource" => {
                // A resource_link cannot represent both nested and outer _meta.
                // Preserve that JSON instead of flattening or dropping metadata.
                if self.payload.get("_meta").is_some() {
                    return None;
                }
                "blob"
            }
            _ => "data",
        };
        let encoded = self.payload.get(field)?.as_str()?;
        // Malformed payloads stay recoverable as full JSON, never cut base64.
        let bytes = STANDARD.decode(encoded).ok()?;
        // Images use the same full-data-URI hash as worker attachment caches.
        Some((bytes, data_url_cid(session, self.mime(), encoded)))
    }

    fn with_text(&self, text: String) -> Value {
        let mut replacement = self.original.clone();
        let target = match self.kind {
            "resource" => &mut replacement["resource"],
            _ => &mut replacement,
        };
        target["text"] = Value::String(text);
        replacement
    }

    fn preview(&self, text: &str, marker: &str, budget: usize) -> Option<Value> {
        let minimal = self.with_text(marker.to_owned());
        if serialized_len(&minimal) > budget {
            return None;
        }
        let mut preview_budget = budget;
        while preview_budget > 0 {
            let preview = truncate_output(
                text,
                &TruncateOpts {
                    max_output_bytes: preview_budget,
                    ..Default::default()
                },
            );
            let replacement = self.with_text(format!("{preview}{marker}"));
            if serialized_len(&replacement) <= budget {
                return Some(replacement);
            }
            preview_budget /= 2;
        }
        Some(minimal)
    }

    fn link(&self, url: &CidUrl, size: usize) -> Value {
        let mut replacement = json!({"type": "resource_link", "uri": url.to_string(), "name": format!("{} attachment", self.kind), "mimeType": self.mime(), "size": size});
        if let Some(uri) = self.source_uri() {
            replacement["description"] = Value::String(format!("Original resource: {uri}"));
        }
        for field in ["annotations", "_meta"] {
            if let Some(value) = self.original.get(field) {
                replacement[field] = value.clone();
            }
        }
        replacement
    }

    fn source_uri(&self) -> Option<&str> {
        match self.kind {
            "resource" => self.payload.get("uri").and_then(Value::as_str),
            _ => None,
        }
    }
}

pub(super) async fn replace_part(
    original: &Value,
    budget: usize,
    session: &SessionRef,
    uploads: &AttachmentUploads<'_>,
) -> Result<Option<Value>, ToolInvokeError> {
    let Some(part) = Part::parse(original) else {
        return Ok(None);
    };
    let mime_type = part.mime();
    let (bytes, url, replacement) = if let Some(text) = part.text() {
        let url = data_url_cid(session, mime_type, &STANDARD.encode(text));
        let marker = format!("\nOriginal ({mime_type}): {url}. Read with attachment_read using byte_offset/byte_limit, or narrow the tool request.");
        (
            text.as_bytes().to_vec(),
            url,
            part.preview(text, &marker, budget),
        )
    } else {
        let Some((bytes, url)) = part.binary(session) else {
            return Ok(None);
        };
        let link = part.link(&url, bytes.len());
        let replacement = (serialized_len(&link) <= budget).then_some(link);
        (bytes, url, replacement)
    };
    let Some(replacement) = replacement else {
        return Ok(None);
    };
    uploads
        .put(MediaBlob {
            url: &url,
            bytes: &bytes,
            mime_type,
        })
        .await?;
    Ok(Some(replacement))
}

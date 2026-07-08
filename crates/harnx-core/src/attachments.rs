//! Shared attachment expansion/cache scaffolding used by runtime and client
//! crates. Runtime owns session-local storage and provider-specific wiring;
//! core owns pure shared types and helpers that must not depend on runtime.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

use crate::crypto::sha256;
use crate::message::{Message, MessageContent, MessageContentPart, MessageContentToolCalls};

/// Prefix marking a content reference in `ImageUrl.url`.
pub const CID_PREFIX: &str = "cid:";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpandedAttachment {
    DataUri {
        data: String,
        mime_type: String,
    },
    RemoteRef {
        ref_id: String,
        mime_type: String,
        expires_at: Option<DateTime<Utc>>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedRef {
    pub ref_id: String,
    pub mime_type: String,
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default)]
pub struct AttachmentRefCache {
    inner: Arc<Mutex<HashMap<String, CachedRef>>>,
}

impl AttachmentRefCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get_valid(&self, cid: &str, now: DateTime<Utc>) -> Option<CachedRef> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let entry = guard.get(cid)?;
        match entry.expires_at {
            Some(expires_at) if expires_at <= now => None,
            _ => Some(entry.clone()),
        }
    }

    pub fn insert(&self, cid: String, entry: CachedRef) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(cid, entry);
    }
}

/// Process-global, provider-scoped attachment cache store.
/// Survives across turns because clients are recreated per-turn.
/// Key is the client's configured provider/name (e.g., "gemini/my-gemini").
static SHARED_CACHES: OnceLock<Mutex<HashMap<String, AttachmentRefCache>>> = OnceLock::new();

/// Returns the shared attachment cache for a given provider scope.
/// The scope is typically the client's configured name (e.g., "gemini/my-gemini").
/// The same scope always returns the same cache instance, persisting across turns.
pub fn shared_attachment_cache(scope: &str) -> AttachmentRefCache {
    let caches = SHARED_CACHES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = caches.lock().unwrap_or_else(|e| e.into_inner());
    guard.entry(scope.to_string()).or_default().clone()
}

pub fn expand_passthrough_reference(reference: &str) -> ExpandedAttachment {
    if let Some((mime_type, data)) = reference
        .strip_prefix("data:")
        .and_then(|rest| rest.split_once(";base64,"))
    {
        ExpandedAttachment::DataUri {
            data: data.to_string(),
            mime_type: mime_type.to_string(),
        }
    } else {
        ExpandedAttachment::RemoteRef {
            ref_id: reference.to_string(),
            mime_type: String::new(),
            expires_at: None,
        }
    }
}

pub fn store_attachment_bytes(dir: &Path, bytes: &[u8], mime_type: &str) -> Result<String> {
    let ext = mime_type
        .split(';')
        .next()
        .unwrap_or("application/octet-stream")
        .trim()
        .to_ascii_lowercase();
    let ext = match ext.as_str() {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        _ => "bin",
    };
    let data = format!(
        "data:{mime_type};base64,{}",
        crate::crypto::base64_encode(bytes)
    );
    let cid = cid_for_data_url(&data);
    std::fs::create_dir_all(dir)
        .with_context(|| format!("Failed to create attachments dir {}", dir.display()))?;
    let hash = cid.trim_start_matches(CID_PREFIX);
    let path = dir.join(format!("{hash}.{ext}"));
    if !path.exists() {
        std::fs::write(&path, bytes)
            .with_context(|| format!("Failed to write attachment {}", path.display()))?;
    }
    Ok(cid)
}

pub fn read_attachment(dir: &Path, reference: &str) -> Result<(Vec<u8>, String)> {
    let hash = reference.strip_prefix(CID_PREFIX).ok_or_else(|| {
        anyhow::anyhow!("attachment reference must start with {CID_PREFIX}: {reference}")
    })?;

    let path = std::fs::read_dir(dir)
        .with_context(|| format!("Failed to read attachments dir {}", dir.display()))?
        .flatten()
        .find_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let stem = name.rsplit_once('.').map_or(name.as_str(), |(s, _)| s);
            (stem == hash).then_some(entry.path())
        })
        .ok_or_else(|| anyhow::anyhow!("Attachment blob not found for {reference}"))?;

    let data = std::fs::read(&path)
        .with_context(|| format!("Failed to read attachment {}", path.display()))?;

    let mime_type = match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => "application/octet-stream",
    }
    .to_string();

    Ok((data, mime_type))
}

pub fn cid_for_data_url(data_url: &str) -> String {
    format!("{CID_PREFIX}{}", sha256(data_url))
}

pub fn collect_cid_refs(messages: &[Message]) -> Vec<String> {
    let mut refs = Vec::new();
    for message in messages {
        match &message.content {
            MessageContent::Array(parts) => collect_cid_refs_from_parts(parts, &mut refs),
            MessageContent::ToolCalls(MessageContentToolCalls { tool_results, .. }) => {
                for tool_result in tool_results {
                    collect_cid_refs_from_parts(&tool_result.content, &mut refs);
                }
            }
            MessageContent::Text(_) => {}
        }
    }
    refs.sort();
    refs.dedup();
    refs
}

fn collect_cid_refs_from_parts(parts: &[MessageContentPart], refs: &mut Vec<String>) {
    for part in parts {
        if let MessageContentPart::ImageUrl { image_url } = part {
            if image_url.url.starts_with(CID_PREFIX) {
                refs.push(image_url.url.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    use crate::message::{ImageUrl, MessageRole};
    use crate::tool::{ToolCall, ToolResult};

    #[test]
    fn attachment_cache_hit_without_expiry() {
        let cache = AttachmentRefCache::new();
        let now = Utc::now();
        let entry = CachedRef {
            ref_id: "files/123".into(),
            mime_type: "image/png".into(),
            expires_at: None,
        };

        cache.insert("cid:abc".into(), entry.clone());

        assert_eq!(cache.get_valid("cid:abc", now), Some(entry));
    }

    #[test]
    fn attachment_cache_expired_entry_is_invalid() {
        let cache = AttachmentRefCache::new();
        let now = Utc::now();
        cache.insert(
            "cid:expired".into(),
            CachedRef {
                ref_id: "files/old".into(),
                mime_type: "image/png".into(),
                expires_at: Some(now - Duration::seconds(1)),
            },
        );

        assert_eq!(cache.get_valid("cid:expired", now), None);
    }

    #[test]
    fn attachment_cache_future_expiry_is_valid() {
        let cache = AttachmentRefCache::new();
        let now = Utc::now();
        let entry = CachedRef {
            ref_id: "files/fresh".into(),
            mime_type: "image/webp".into(),
            expires_at: Some(now + Duration::hours(1)),
        };
        cache.insert("cid:fresh".into(), entry.clone());

        assert_eq!(cache.get_valid("cid:fresh", now), Some(entry));
    }

    #[test]
    fn store_attachment_bytes_round_trips_via_read_attachment() {
        let tmp = tempfile::tempdir().unwrap();
        let cid = store_attachment_bytes(tmp.path(), b"hello attachment", "image/png").unwrap();
        assert!(cid.starts_with(CID_PREFIX));

        let (bytes, mime_type) = read_attachment(tmp.path(), &cid).unwrap();
        assert_eq!(bytes, b"hello attachment");
        assert_eq!(mime_type, "image/png");
    }
    #[test]
    fn attachment_cache_missing_cid_is_none() {
        let cache = AttachmentRefCache::new();
        assert_eq!(cache.get_valid("cid:missing", Utc::now()), None);
    }

    #[test]
    fn passthrough_data_url_expands_inline() {
        assert_eq!(
            expand_passthrough_reference("data:image/png;base64,QUJD"),
            ExpandedAttachment::DataUri {
                data: "QUJD".into(),
                mime_type: "image/png".into(),
            }
        );
    }

    #[test]
    fn passthrough_remote_url_keeps_ref() {
        assert_eq!(
            expand_passthrough_reference("https://example.com/image.png"),
            ExpandedAttachment::RemoteRef {
                ref_id: "https://example.com/image.png".into(),
                mime_type: String::new(),
                expires_at: None,
            }
        );
    }

    #[test]
    fn cid_for_data_url_is_stable() {
        let cid = cid_for_data_url("data:image/png;base64,QUJD");
        assert!(cid.starts_with(CID_PREFIX));
        assert_eq!(cid, cid_for_data_url("data:image/png;base64,QUJD"));
    }

    #[test]
    fn read_attachment_round_trips_mime() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("deadbeef.png");
        std::fs::write(&path, b"ABC").unwrap();

        let (data, mime_type) = read_attachment(tmp.path(), "cid:deadbeef").unwrap();
        assert_eq!(data, b"ABC");
        assert_eq!(mime_type, "image/png");
    }

    #[test]
    fn read_attachment_requires_cid_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let err = read_attachment(tmp.path(), "https://example.com/image.png").unwrap_err();
        assert!(err
            .to_string()
            .contains("attachment reference must start with cid:"));
    }

    #[test]
    fn read_attachment_missing_blob_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let err = read_attachment(tmp.path(), "cid:missing").unwrap_err();
        assert!(err.to_string().contains("Attachment blob not found"));
    }

    #[test]
    fn read_attachment_unknown_extension_uses_octet_stream() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("deadbeef.bin");
        std::fs::write(&path, b"ABC").unwrap();

        let (_, mime_type) = read_attachment(tmp.path(), "cid:deadbeef").unwrap();
        assert_eq!(mime_type, "application/octet-stream");
    }

    #[test]
    fn cid_for_data_url_uses_full_data_url() {
        let a = cid_for_data_url("data:image/png;base64,QUJD");
        let b = cid_for_data_url("data:image/jpeg;base64,QUJD");
        assert_ne!(a, b);
    }

    #[test]
    fn collect_cid_refs_scans_arrays_and_tool_results_and_dedups() {
        let cid_a = "cid:aaa".to_string();
        let cid_b = "cid:bbb".to_string();
        let tool_call = ToolCall::new("show".to_string(), serde_json::json!({}), None, None);
        let mut tool_result = ToolResult::new(tool_call, serde_json::json!({"ok": true}));
        tool_result.content = vec![
            MessageContentPart::ImageUrl {
                image_url: ImageUrl { url: cid_b.clone() },
            },
            MessageContentPart::ImageUrl {
                image_url: ImageUrl { url: cid_a.clone() },
            },
        ];

        let messages = vec![
            Message {
                role: MessageRole::User,
                content: MessageContent::Array(vec![
                    MessageContentPart::ImageUrl {
                        image_url: ImageUrl { url: cid_a.clone() },
                    },
                    MessageContentPart::ImageUrl {
                        image_url: ImageUrl {
                            url: "https://example.com/image.png".into(),
                        },
                    },
                ]),
                ..Default::default()
            },
            Message {
                role: MessageRole::Tool,
                content: MessageContent::ToolCalls(MessageContentToolCalls::new(
                    vec![tool_result],
                    String::new(),
                    None,
                )),
                ..Default::default()
            },
        ];

        assert_eq!(collect_cid_refs(&messages), vec![cid_a, cid_b]);
    }

    #[test]
    fn shared_cache_same_scope_returns_same_instance() {
        let cache1 = shared_attachment_cache("test-scope-a");
        let cache2 = shared_attachment_cache("test-scope-a");

        // Both caches are clones sharing the same inner Arc
        cache1.insert(
            "cid:abc".into(),
            CachedRef {
                ref_id: "https://example.com/file1".into(),
                mime_type: "image/png".into(),
                expires_at: None,
            },
        );

        // cache2 should see the insertion because they share state
        let entry = cache2.get_valid("cid:abc", Utc::now());
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().ref_id, "https://example.com/file1");
    }

    #[test]
    fn shared_cache_different_scopes_are_isolated() {
        let cache_a = shared_attachment_cache("test-scope-b");
        let cache_b = shared_attachment_cache("test-scope-c");

        cache_a.insert(
            "cid:shared".into(),
            CachedRef {
                ref_id: "https://example.com/a".into(),
                mime_type: "image/png".into(),
                expires_at: None,
            },
        );

        cache_b.insert(
            "cid:shared".into(),
            CachedRef {
                ref_id: "https://example.com/b".into(),
                mime_type: "image/jpeg".into(),
                expires_at: None,
            },
        );

        // Each scope has its own isolated cache
        let entry_a = cache_a.get_valid("cid:shared", Utc::now()).unwrap();
        let entry_b = cache_b.get_valid("cid:shared", Utc::now()).unwrap();

        assert_eq!(entry_a.ref_id, "https://example.com/a");
        assert_eq!(entry_b.ref_id, "https://example.com/b");
        assert_ne!(entry_a.mime_type, entry_b.mime_type);
    }
}

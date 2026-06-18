use std::path::Path;

use anyhow::{Context, Result};
use chrono::Utc;
use harnx_core::attachments::{
    expand_passthrough_reference, read_attachment, AttachmentRefCache, CachedRef,
    ExpandedAttachment, CID_PREFIX,
};
use harnx_core::crypto::base64_encode;
use reqwest::multipart::{Form, Part};
use serde::Deserialize;

pub const ANTHROPIC_FILES_BETA_HEADER_VALUE: &str = "files-api-2025-04-14";
const DEFAULT_UPLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Clone)]
pub struct AnthropicAttachmentEncoder {
    pub cache: AttachmentRefCache,
    pub http: reqwest::Client,
    pub api_key: String,
    pub api_base: String,
    pub upload_timeout: std::time::Duration,
}

impl AnthropicAttachmentEncoder {
    pub fn new(http: reqwest::Client, api_key: String, api_base: String) -> Self {
        Self {
            cache: AttachmentRefCache::new(),
            http,
            api_key,
            api_base,
            upload_timeout: DEFAULT_UPLOAD_TIMEOUT,
        }
    }

    pub fn new_with_cache(
        http: reqwest::Client,
        api_key: String,
        api_base: String,
        cache: AttachmentRefCache,
    ) -> Self {
        Self {
            cache,
            http,
            api_key,
            api_base,
            upload_timeout: DEFAULT_UPLOAD_TIMEOUT,
        }
    }

    #[cfg(test)]
    pub fn with_upload_timeout(mut self, upload_timeout: std::time::Duration) -> Self {
        self.upload_timeout = upload_timeout;
        self
    }

    pub async fn expand(&self, dir: &Path, reference: &str) -> Result<ExpandedAttachment> {
        if !reference.starts_with(CID_PREFIX) {
            return Ok(expand_passthrough_reference(reference));
        }

        let (bytes, mime_type) = read_attachment(dir, reference)?;

        if let Some(cached) = self.cache.get_valid(reference, Utc::now()) {
            return Ok(ExpandedAttachment::RemoteRef {
                ref_id: cached.ref_id,
                mime_type: cached.mime_type,
                expires_at: cached.expires_at,
            });
        }

        match self.upload(reference, &bytes, &mime_type).await {
            Ok(expanded) => Ok(expanded),
            Err(err) => {
                warn!(
                    "Anthropic attachment upload failed for {}: {}",
                    reference, err
                );
                Ok(ExpandedAttachment::DataUri {
                    data: base64_encode(&bytes),
                    mime_type,
                })
            }
        }
    }

    async fn upload(&self, cid: &str, bytes: &[u8], mime_type: &str) -> Result<ExpandedAttachment> {
        let filename = format!(
            "{}.{}",
            cid.trim_start_matches(CID_PREFIX),
            extension_for_mime(mime_type)
        );
        let part = Part::bytes(bytes.to_vec())
            .file_name(filename)
            .mime_str(mime_type)
            .context("Invalid attachment mime type for Anthropic upload")?;
        let form = Form::new().part("file", part);

        let url = format!("{}/files", self.api_base.trim_end_matches('/'));
        let mut request = self
            .http
            .post(url)
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", ANTHROPIC_FILES_BETA_HEADER_VALUE)
            .multipart(form);

        if self.api_key.starts_with("sk-ant-oat") {
            request = request.bearer_auth(&self.api_key);
        } else {
            request = request.header("x-api-key", &self.api_key);
        }

        let response = request.timeout(self.upload_timeout).send().await?;
        let response = response.error_for_status()?;
        let uploaded: AnthropicFileUploadResponse = response
            .json()
            .await
            .context("Invalid Anthropic files API response")?;

        let cached = CachedRef {
            ref_id: uploaded.id,
            mime_type: uploaded.mime_type,
            expires_at: None,
        };
        self.cache.insert(cid.to_string(), cached.clone());

        Ok(ExpandedAttachment::RemoteRef {
            ref_id: cached.ref_id,
            mime_type: cached.mime_type,
            expires_at: cached.expires_at,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct AnthropicFileUploadResponse {
    id: String,
    mime_type: String,
}

fn extension_for_mime(mime_type: &str) -> &'static str {
    match mime_type {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "bin",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[tokio::test]
    async fn anthropic_expand_non_cid_passthrough() {
        let encoder = AnthropicAttachmentEncoder::new(
            reqwest::Client::new(),
            "test-key".into(),
            "https://api.anthropic.com/v1".into(),
        );

        let expanded = encoder
            .expand(Path::new("."), "data:image/png;base64,QUJD")
            .await
            .unwrap();

        assert_eq!(
            expanded,
            ExpandedAttachment::DataUri {
                data: "QUJD".into(),
                mime_type: "image/png".into(),
            }
        );
    }

    #[tokio::test]
    async fn anthropic_expand_cache_hit_returns_remote_ref() {
        let cache = AttachmentRefCache::new();
        let expires_at = Utc::now() + Duration::hours(1);
        cache.insert(
            "cid:cached".into(),
            CachedRef {
                ref_id: "file_123".into(),
                mime_type: "image/png".into(),
                expires_at: Some(expires_at),
            },
        );
        let encoder = AnthropicAttachmentEncoder::new_with_cache(
            reqwest::Client::new(),
            "test-key".into(),
            "https://api.anthropic.com/v1".into(),
            cache,
        );
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("cached.png"), b"PNG").unwrap();

        let expanded = encoder.expand(tmp.path(), "cid:cached").await.unwrap();

        assert_eq!(
            expanded,
            ExpandedAttachment::RemoteRef {
                ref_id: "file_123".into(),
                mime_type: "image/png".into(),
                expires_at: Some(expires_at),
            }
        );
    }

    #[tokio::test]
    async fn anthropic_expand_timeout_falls_back_to_data_uri() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        use tokio::time::{sleep, Duration};

        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("timeout.png"), b"PNG").unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = [0u8; 1024];
            let _ = stream.read(&mut buffer).await;
            sleep(Duration::from_millis(250)).await;
            let _ = stream.shutdown().await;
        });

        let encoder = AnthropicAttachmentEncoder::new(
            reqwest::Client::new(),
            "test-key".into(),
            format!("http://{addr}"),
        )
        .with_upload_timeout(Duration::from_millis(50));

        let started = std::time::Instant::now();
        let expanded = encoder.expand(tmp.path(), "cid:timeout").await.unwrap();

        assert!(
            started.elapsed() < Duration::from_secs(1),
            "timeout fallback should return quickly"
        );
        assert_eq!(
            expanded,
            ExpandedAttachment::DataUri {
                data: base64_encode(b"PNG"),
                mime_type: "image/png".into(),
            }
        );

        server.await.unwrap();
    }
}

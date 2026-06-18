use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use harnx_core::attachments::{
    expand_passthrough_reference, read_attachment, AttachmentRefCache, CachedRef,
    ExpandedAttachment, CID_PREFIX,
};
use harnx_core::crypto::base64_encode;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_LENGTH, CONTENT_TYPE};
use serde::{Deserialize, Serialize};

const DEFAULT_UPLOAD_API_BASE: &str = "https://generativelanguage.googleapis.com/upload/v1beta";
const HEADER_X_GOOG_API_KEY: &str = "x-goog-api-key";
const HEADER_X_GOOG_UPLOAD_URL: &str = "x-goog-upload-url";
const DEFAULT_UPLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Clone)]
pub struct GeminiAttachmentEncoder {
    pub cache: AttachmentRefCache,
    pub http: reqwest::Client,
    pub api_key: String,
    pub api_base: String,
    pub upload_timeout: std::time::Duration,
}

impl GeminiAttachmentEncoder {
    pub fn new(http: reqwest::Client, api_key: String, api_base: String) -> Self {
        Self {
            cache: AttachmentRefCache::new(),
            http,
            api_key,
            api_base,
            upload_timeout: DEFAULT_UPLOAD_TIMEOUT,
        }
    }

    /// Creates an encoder with a pre-existing cache (for sharing across turns).
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
        if let Some(entry) = self.cache.get_valid(reference, Utc::now()) {
            return Ok(ExpandedAttachment::RemoteRef {
                ref_id: entry.ref_id,
                mime_type: entry.mime_type,
                expires_at: entry.expires_at,
            });
        }

        match self.upload(reference, &bytes, &mime_type).await {
            Ok(remote) => Ok(remote),
            Err(err) => {
                warn!("Gemini upload failed for {reference}; falling back to data URI: {err}");
                Ok(ExpandedAttachment::DataUri {
                    data: base64_encode(&bytes),
                    mime_type,
                })
            }
        }
    }

    async fn upload(&self, cid: &str, bytes: &[u8], mime_type: &str) -> Result<ExpandedAttachment> {
        let upload_url = self.start_upload(cid, bytes.len(), mime_type).await?;
        let uploaded_at = Utc::now();
        let response = self
            .finalize_upload(&upload_url, bytes)
            .await
            .context("Gemini upload finalize failed")?;
        let file = response.file;
        let expires_at = match file.expiration_time.as_deref() {
            Some(raw) => Some(
                DateTime::parse_from_rfc3339(raw)
                    .with_context(|| format!("invalid Gemini expirationTime {raw}"))?
                    .with_timezone(&Utc),
            ),
            None => Some(uploaded_at + Duration::hours(47)),
        };
        let cached = CachedRef {
            ref_id: file.uri.clone(),
            mime_type: file.mime_type.clone(),
            expires_at,
        };
        self.cache.insert(cid.to_string(), cached.clone());
        Ok(ExpandedAttachment::RemoteRef {
            ref_id: cached.ref_id,
            mime_type: cached.mime_type,
            expires_at: cached.expires_at,
        })
    }

    async fn start_upload(&self, cid: &str, bytes_len: usize, mime_type: &str) -> Result<String> {
        let response = self
            .http
            .post(upload_api_base(&self.api_base))
            .headers(start_headers(&self.api_key, bytes_len, mime_type)?)
            .json(&GeminiStartUploadRequest {
                file: GeminiStartUploadFile {
                    display_name: cid.to_string(),
                },
            })
            .timeout(self.upload_timeout)
            .send()
            .await
            .context("Gemini upload start request failed")?
            .error_for_status()
            .context("Gemini upload start returned error status")?;
        let header = response
            .headers()
            .get(HEADER_X_GOOG_UPLOAD_URL)
            .context("Gemini upload start missing x-goog-upload-url header")?;
        Ok(header
            .to_str()
            .context("Gemini upload start x-goog-upload-url header not utf-8")?
            .to_string())
    }

    async fn finalize_upload(
        &self,
        upload_url: &str,
        bytes: &[u8],
    ) -> Result<GeminiUploadResponse> {
        self.http
            .post(upload_url)
            .headers(finalize_headers(bytes.len())?)
            .body(bytes.to_vec())
            .timeout(self.upload_timeout)
            .send()
            .await
            .context("Gemini upload finalize request failed")?
            .error_for_status()
            .context("Gemini upload finalize returned error status")?
            .json::<GeminiUploadResponse>()
            .await
            .context("Gemini upload finalize JSON parse failed")
    }
}

fn upload_api_base(api_base: &str) -> String {
    let trimmed = api_base.trim_end_matches('/');
    if trimmed.ends_with("/upload/v1beta") {
        return trimmed.to_string();
    }
    if let Some(prefix) = trimmed.strip_suffix("/v1beta") {
        return format!("{prefix}/upload/v1beta");
    }
    if trimmed.contains("/upload/") {
        return trimmed.to_string();
    }
    if trimmed.is_empty() {
        return DEFAULT_UPLOAD_API_BASE.to_string();
    }
    format!("{trimmed}/upload/v1beta")
}

fn start_headers(api_key: &str, bytes_len: usize, mime_type: &str) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(HEADER_X_GOOG_API_KEY, HeaderValue::from_str(api_key)?);
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        HeaderName::from_static("x-goog-upload-protocol"),
        HeaderValue::from_static("resumable"),
    );
    headers.insert(
        HeaderName::from_static("x-goog-upload-command"),
        HeaderValue::from_static("start"),
    );
    headers.insert(
        HeaderName::from_static("x-goog-upload-header-content-length"),
        HeaderValue::from_str(&bytes_len.to_string())?,
    );
    headers.insert(
        HeaderName::from_static("x-goog-upload-header-content-type"),
        HeaderValue::from_str(mime_type)?,
    );
    Ok(headers)
}

fn finalize_headers(bytes_len: usize) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&bytes_len.to_string())?,
    );
    headers.insert(
        HeaderName::from_static("x-goog-upload-offset"),
        HeaderValue::from_static("0"),
    );
    headers.insert(
        HeaderName::from_static("x-goog-upload-command"),
        HeaderValue::from_static("upload, finalize"),
    );
    Ok(headers)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiStartUploadRequest {
    file: GeminiStartUploadFile,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiStartUploadFile {
    display_name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiUploadResponse {
    pub file: GeminiUploadedFile,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiUploadedFile {
    pub uri: String,
    pub mime_type: String,
    pub expiration_time: Option<String>,
    pub name: String,
    pub state: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn expand_non_cid_passthrough_uses_core_helper() {
        let encoder = GeminiAttachmentEncoder::new(
            reqwest::Client::new(),
            "key".into(),
            "https://generativelanguage.googleapis.com/v1beta".into(),
        );

        let expanded = encoder
            .expand(Path::new("."), "https://example.com/image.png")
            .await
            .unwrap();

        assert_eq!(
            expanded,
            ExpandedAttachment::RemoteRef {
                ref_id: "https://example.com/image.png".into(),
                mime_type: String::new(),
                expires_at: None,
            }
        );
    }

    #[tokio::test]
    async fn expand_cache_hit_skips_network() {
        let encoder = GeminiAttachmentEncoder::new(
            reqwest::Client::new(),
            "key".into(),
            "https://generativelanguage.googleapis.com/v1beta".into(),
        );
        let tmp = tempfile::tempdir().unwrap();
        let cid = "cid:deadbeef";
        std::fs::write(tmp.path().join("deadbeef.png"), b"PNG").unwrap();
        let expires_at = Some(Utc::now() + Duration::hours(1));
        encoder.cache.insert(
            cid.into(),
            CachedRef {
                ref_id: "https://files.example/abc".into(),
                mime_type: "image/png".into(),
                expires_at,
            },
        );

        let expanded = encoder.expand(tmp.path(), cid).await.unwrap();

        assert_eq!(
            expanded,
            ExpandedAttachment::RemoteRef {
                ref_id: "https://files.example/abc".into(),
                mime_type: "image/png".into(),
                expires_at,
            }
        );
    }

    #[test]
    fn upload_api_base_inserts_upload_segment_before_v1beta() {
        assert_eq!(
            upload_api_base("https://generativelanguage.googleapis.com/v1beta"),
            "https://generativelanguage.googleapis.com/upload/v1beta"
        );
    }

    #[test]
    fn upload_api_base_preserves_existing_upload_base() {
        assert_eq!(
            upload_api_base("https://example.com/custom/upload/v1beta"),
            "https://example.com/custom/upload/v1beta"
        );
    }

    #[test]
    fn upload_api_base_handles_custom_nonstandard_base() {
        assert_eq!(
            upload_api_base("https://example.com/custom-api"),
            "https://example.com/custom-api/upload/v1beta"
        );
    }

    #[tokio::test]
    async fn expand_timeout_falls_back_to_data_uri() {
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

        let encoder = GeminiAttachmentEncoder::new(
            reqwest::Client::new(),
            "key".into(),
            format!("http://{addr}/v1beta"),
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

    #[test]
    fn upload_response_allows_missing_expiration_time() {
        let response: GeminiUploadResponse = serde_json::from_str(
            r#"{"file":{"uri":"https://files.example/1","mimeType":"image/png","name":"files/1","state":"ACTIVE"}}"#,
        )
        .unwrap();

        assert_eq!(response.file.expiration_time, None);
        assert_eq!(response.file.uri, "https://files.example/1");
    }
}

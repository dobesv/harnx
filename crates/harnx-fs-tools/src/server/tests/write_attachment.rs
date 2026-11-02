use super::*;
use crate::{AttachmentResolveError, AttachmentResolver, FsToolset};
use harnx_blob_store::BlobTooLarge;
use harnx_toolset::ToolInvokeError;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio_util::sync::CancellationToken;

const PLAN_URL: &str = "cid:plan:_temp/abcdef/test";

fn media_url() -> String {
    format!("cid:media:_temp/abcdef/{}", "a".repeat(64))
}

struct FakeResolver {
    url: String,
    response: Mutex<Option<Result<Vec<u8>, AttachmentResolveError>>>,
    calls: AtomicUsize,
}

impl FakeResolver {
    fn new(url: &str, response: Result<Vec<u8>, AttachmentResolveError>) -> Arc<Self> {
        Arc::new(Self {
            url: url.to_string(),
            response: Mutex::new(Some(response)),
            calls: AtomicUsize::new(0),
        })
    }
}

#[async_trait::async_trait]
impl AttachmentResolver for FakeResolver {
    async fn resolve(
        &self,
        url: &str,
        max_bytes: usize,
    ) -> Result<Vec<u8>, AttachmentResolveError> {
        assert_eq!(url, self.url);
        assert_eq!(max_bytes, WRITE_MAX_BYTES);
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.response.lock().unwrap().take().unwrap()
    }
}

fn attachment_args(path: &Path, url: &str) -> Value {
    json!({"path": path_string(path), "url": url})
}

async fn native_write(
    toolset: &FsToolset,
    path: &Path,
    url: &str,
) -> Result<Value, ToolInvokeError> {
    toolset
        .invoke(
            "write_attachment",
            attachment_args(path, url),
            CancellationToken::new(),
        )
        .await
}

fn recoverable_message(result: Result<Value, ToolInvokeError>) -> String {
    match result {
        Err(ToolInvokeError::Recoverable(message)) => message,
        other => panic!("expected recoverable error, got {other:?}"),
    }
}

#[tokio::test]
async fn native_write_attachment_preserves_binary_empty_and_plan_bytes() {
    let root = TestDir::new();
    let media = media_url();
    for (name, url, bytes) in [
        (
            "nested/image.png",
            media.as_str(),
            b"\x89PNG\r\n\x1a\n\xff\0".as_slice(),
        ),
        ("empty.bin", media.as_str(), b"".as_slice()),
        (
            "plan.md",
            PLAN_URL,
            "# Plan\r\n\nExact café Markdown\n".as_bytes(),
        ),
    ] {
        let resolver = FakeResolver::new(url, Ok(bytes.to_vec()));
        let toolset = FsToolset::new(rwx_allowlist([root.path().to_path_buf()]))
            .with_attachment_resolver(resolver.clone());
        let path = root.path().join(name);
        let result = native_write(&toolset, &path, url).await.unwrap();
        assert_eq!(result["isError"], false);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert!(result["_meta"][EXECUTION_CONTEXT_NAMESPACE].is_object());
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn write_attachment_overwrites_and_empty_attachment_truncates() {
    let root = TestDir::new();
    let path = write_fixture(&root, "existing.bin", "original file is longer");
    for bytes in [b"\xff\0".as_slice(), b"".as_slice()] {
        let resolver = FakeResolver::new(PLAN_URL, Ok(bytes.to_vec()));
        let toolset = FsToolset::new(rwx_allowlist([root.path().to_path_buf()]))
            .with_attachment_resolver(resolver);
        let result = native_write(&toolset, &path, PLAN_URL).await.unwrap();
        assert_eq!(result["isError"], false);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
}

#[tokio::test]
async fn write_attachment_denies_destination_before_resolving() {
    let root = TestDir::new();
    let outside = TestDir::new();
    let path = write_fixture(&outside, "existing.bin", "unchanged");
    let resolver = FakeResolver::new(PLAN_URL, Ok(vec![0, 255]));
    let mut allowlist = rwx_allowlist([root.path().to_path_buf()]);
    allowlist.insert_read(outside.path());
    let toolset = FsToolset::new(allowlist).with_attachment_resolver(resolver.clone());
    let message = recoverable_message(native_write(&toolset, &path, PLAN_URL).await);
    assert!(message.contains("outside allowed write paths"), "{message}");
    assert_eq!(std::fs::read(&path).unwrap(), b"unchanged");
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn write_attachment_rejects_blank_and_invalid_cids_without_resolving() {
    let root = TestDir::new();
    let path = write_fixture(&root, "existing.bin", "unchanged");
    let resolver = FakeResolver::new(PLAN_URL, Ok(vec![0, 255]));
    let toolset = FsToolset::new(rwx_allowlist([root.path().to_path_buf()]))
        .with_attachment_resolver(resolver.clone());
    for url in [
        "",
        " \t\n",
        "https://example.com",
        "cid:bad:value",
        "cid:media:_temp/abcdef/short",
    ] {
        let message = recoverable_message(native_write(&toolset, &path, url).await);
        assert!(message.starts_with("Invalid attachment URL:"), "{message}");
        assert_eq!(std::fs::read(&path).unwrap(), b"unchanged");
    }
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn write_attachment_without_resolver_is_recoverable_and_leaves_disk_untouched() {
    let root = TestDir::new();
    let path = write_fixture(&root, "existing.bin", "unchanged");
    let toolset = FsToolset::new(rwx_allowlist([root.path().to_path_buf()]));
    for destination in [path.clone(), root.path().join("missing/nested/file.bin")] {
        let message = recoverable_message(native_write(&toolset, &destination, PLAN_URL).await);
        assert_eq!(
            message,
            "Attachment resolution unavailable: no resolver configured for this filesystem server."
        );
    }
    assert_eq!(std::fs::read(&path).unwrap(), b"unchanged");
    assert!(!root.path().join("missing").exists());
    let invalid = recoverable_message(native_write(&toolset, &path, "").await);
    assert!(invalid.starts_with("Invalid attachment URL:"));
}

fn resolution_failures() -> Vec<(AttachmentResolveError, String)> {
    vec![
        (
            AttachmentResolveError::NotFound(anyhow::anyhow!("attachment not found: {PLAN_URL}")),
            "not found".to_string(),
        ),
        (
            AttachmentResolveError::TooLarge(BlobTooLarge {
                size_bytes: WRITE_MAX_BYTES + 1,
                max_bytes: WRITE_MAX_BYTES,
            }),
            format!(
                "{} bytes exceeds limit of {} bytes",
                WRITE_MAX_BYTES + 1,
                WRITE_MAX_BYTES
            ),
        ),
        (
            AttachmentResolveError::Other(anyhow::anyhow!("storage offline")),
            "Attachment resolution failed: storage offline".to_string(),
        ),
        (
            AttachmentResolveError::InvalidUrl(anyhow::anyhow!("resolver rejected URL")),
            "Invalid attachment URL: resolver rejected URL".to_string(),
        ),
    ]
}

#[tokio::test]
async fn write_attachment_resolution_errors_never_change_destination_or_create_parents() {
    let root = TestDir::new();
    let path = write_fixture(&root, "existing.bin", b"\xfforiginal\0");
    for (error, expected) in resolution_failures() {
        let resolver = FakeResolver::new(PLAN_URL, Err(error));
        let toolset = FsToolset::new(rwx_allowlist([root.path().to_path_buf()]))
            .with_attachment_resolver(resolver.clone());
        let message = recoverable_message(native_write(&toolset, &path, PLAN_URL).await);
        assert!(message.contains(&expected), "{message}");
        assert_eq!(std::fs::read(&path).unwrap(), b"\xfforiginal\0");
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    }
    let missing = root.path().join("missing/nested/file.bin");
    let resolver = FakeResolver::new(
        PLAN_URL,
        Err(AttachmentResolveError::NotFound(anyhow::anyhow!(
            "plan document not found: {PLAN_URL}"
        ))),
    );
    let toolset = FsToolset::new(rwx_allowlist([root.path().to_path_buf()]))
        .with_attachment_resolver(resolver);
    recoverable_message(native_write(&toolset, &missing, PLAN_URL).await);
    assert!(!root.path().join("missing").exists());
}

#[tokio::test]
async fn write_attachment_resolves_before_taking_repo_or_file_locks() {
    let root = TestDir::new();
    let path = write_fixture(&root, "existing.bin", "unchanged")
        .canonicalize()
        .unwrap();
    for lock_repo in [true, false] {
        let resolver = FakeResolver::new(
            PLAN_URL,
            Err(AttachmentResolveError::Other(anyhow::anyhow!(
                "source failed"
            ))),
        );
        let server = make_server(root.path()).with_attachment_resolver(resolver.clone());
        let repo_lock = server.repo_lock_for_path(&path);
        let file_lock = server.lock_for_path(&path);
        let _repo_guard = if lock_repo {
            Some(repo_lock.write().await)
        } else {
            None
        };
        let _file_guard = if lock_repo {
            None
        } else {
            Some(file_lock.lock().await)
        };
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            server.write_attachment_impl(WriteAttachmentParams {
                path: path_string(&path),
                url: PLAN_URL.to_string(),
            }),
        )
        .await
        .expect("source resolution must not wait for destination locks");
        assert!(result.unwrap_err().message.contains("source failed"));
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
        assert_eq!(std::fs::read(&path).unwrap(), b"unchanged");
    }
}

mod protocol;

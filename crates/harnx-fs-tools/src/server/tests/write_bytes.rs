use super::*;
use crate::server::sandbox_transport::WRITE_MAX_BASE64_BYTES;
use crate::FsToolset;
use base64::{engine::general_purpose::STANDARD, Engine};
use harnx_toolset::ToolInvokeError;
use tokio_util::sync::CancellationToken;

fn pod_toolset(root: &Path) -> FsToolset {
    FsToolset::new(rwx_allowlist([root.to_path_buf()])).with_sandbox_transport()
}

fn byte_args(path: &Path, encoded: String) -> Value {
    json!({"path": path_string(path), "data_base64": encoded})
}

async fn invoke_bytes(toolset: &FsToolset, args: Value) -> Result<Value, ToolInvokeError> {
    toolset
        .invoke("write_bytes", args, CancellationToken::new())
        .await
}

fn expect_recoverable(result: Result<Value, ToolInvokeError>, expected: &str) {
    match result {
        Err(ToolInvokeError::Recoverable(message)) => {
            assert!(message.contains(expected), "{message}");
        }
        other => panic!("expected recoverable {expected:?}, got {other:?}"),
    }
}

#[tokio::test]
async fn write_bytes_round_trips_binary_text_and_empty_overwrites() {
    let root = TestDir::new();
    let toolset = pod_toolset(root.path());
    let path = root.path().join("nested/payload.bin");
    for bytes in [
        b"\x89PNG\r\n\x1a\n\xff\0".as_slice(),
        "café\r\nexact UTF-8\n".as_bytes(),
        b"".as_slice(),
    ] {
        let result = invoke_bytes(&toolset, byte_args(&path, STANDARD.encode(bytes)))
            .await
            .unwrap();
        assert_ne!(result["isError"], true);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert!(result.to_string().len() < 8192);
    }
}

#[tokio::test]
async fn write_bytes_bad_base64_leaves_existing_file_and_missing_parent_untouched() {
    let root = TestDir::new();
    let path = write_fixture(&root, "existing.bin", "unchanged");
    let missing = root.path().join("not-created/payload.bin");
    let toolset = pod_toolset(root.path());
    for destination in [&path, &missing] {
        for encoded in ["not base64!", "AA", "AA=A", "AB==", "_-=="] {
            expect_recoverable(
                invoke_bytes(&toolset, byte_args(destination, encoded.to_string())).await,
                "Invalid base64 data:",
            );
        }
    }
    assert_eq!(std::fs::read(path).unwrap(), b"unchanged");
    assert!(!missing.parent().unwrap().exists());
}

#[tokio::test]
async fn write_bytes_encoded_oversize_is_rejected_before_decoding_without_mutation() {
    let root = TestDir::new();
    let path = write_fixture(&root, "existing.bin", "unchanged");
    let missing = root.path().join("not-created/payload.bin");
    let toolset = pod_toolset(root.path());
    for destination in [&path, &missing] {
        // Invalid base64 proves the length check runs before decoding.
        let encoded = "!".repeat(WRITE_MAX_BASE64_BYTES + 1);
        expect_recoverable(
            invoke_bytes(&toolset, byte_args(destination, encoded)).await,
            "Encoded content too large",
        );
    }
    assert_eq!(std::fs::read(path).unwrap(), b"unchanged");
    assert!(!missing.parent().unwrap().exists());
}

#[tokio::test]
async fn write_bytes_decoded_oversize_within_encoded_bound_leaves_file_untouched() {
    let root = TestDir::new();
    let path = write_fixture(&root, "existing.bin", "unchanged");
    let mut encoded = "A".repeat(WRITE_MAX_BASE64_BYTES - 1);
    encoded.push('=');
    assert_eq!(encoded.len(), WRITE_MAX_BASE64_BYTES);
    // WRITE_MAX_BYTES % 3 == 1; one padding byte decodes to max + 1 bytes.
    expect_recoverable(
        invoke_bytes(&pod_toolset(root.path()), byte_args(&path, encoded)).await,
        "Decoded content too large",
    );
    assert_eq!(std::fs::read(path).unwrap(), b"unchanged");
}

#[tokio::test]
async fn write_bytes_denied_destination_is_checked_before_decode() {
    let root = TestDir::new();
    let path = write_fixture(&root, "existing.bin", "unchanged");
    let toolset = FsToolset::new(ResolvedAllowlist::new()).with_sandbox_transport();
    let result = invoke_bytes(&toolset, byte_args(&path, "!".to_string())).await;
    expect_recoverable(result, "denied");
    assert_eq!(std::fs::read(path).unwrap(), b"unchanged");
}

#[test]
fn write_bytes_observation_args_retain_only_locations() {
    let args = json!({
        "path": "/workspace/result.bin", "repo_path": "/workspace",
        "data_base64": "sensitive payload", "content": "sensitive text",
    });
    assert_eq!(
        location_args(&args),
        json!({"path": "/workspace/result.bin", "repo_path": "/workspace"}),
    );
    assert_eq!(location_args(&json!({"path": {"secret": true}})), json!({}));
}

mod protocol;

//! Integration tests for Anthropic/Claude Files API upload-by-reference behavior.
//!
//! Uses a network-free in-test mock HTTP server to prove:
//! 1. Upload-once-reuse (same client, 2 turns same image cid ⇒ upload count == 1)
//! 2. Fallback to base64 on upload failure (5xx ⇒ base64 source, no beta header)
//! 3. Bedrock forces base64 (capability false → no upload, inlineData)
//! 4. Anthropic file_ids don't expire (expires_at = None in cache)
//!
//! All tests use unique client names for cache isolation.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use harnx_client::claude::claude_build_chat_completions_body;
use harnx_client::client::{ChatCompletionsData, Client};
use harnx_client::{BedrockClient, ClaudeClient, Model};
use harnx_core::attachments::{shared_attachment_cache, CachedRef, CID_PREFIX};
use harnx_core::message::{ImageUrl, Message, MessageContent, MessageContentPart, MessageRole};
use harnx_core::provider_config::claude::ClaudeConfig;
use http_body_util::BodyExt;
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use parking_lot::RwLock;
use serde_json::json;
use serde_json::Value;
use tokio::net::TcpListener;

// ============================================================================
// Mock Server Infrastructure
// ============================================================================

/// Shared state for the mock server.
#[derive(Debug)]
struct MockState {
    /// Base URL of the mock server.
    base_url: RwLock<String>,
    /// Count of upload requests to /files endpoint.
    upload_count: AtomicUsize,
    /// Count of /messages requests.
    messages_count: AtomicUsize,
    /// Captured /messages request bodies for assertion.
    messages_bodies: RwLock<Vec<Value>>,
    /// Captured /messages request headers for assertion.
    messages_headers: RwLock<Vec<HashMap<String, String>>>,
    /// Inject upload failure (returns 500 on /files).
    fail_upload: RwLock<bool>,
}

impl MockState {
    fn new() -> Self {
        Self {
            base_url: RwLock::new(String::new()),
            upload_count: AtomicUsize::new(0),
            messages_count: AtomicUsize::new(0),
            messages_bodies: RwLock::new(Vec::new()),
            messages_headers: RwLock::new(Vec::new()),
            fail_upload: RwLock::new(false),
        }
    }

    fn upload_count(&self) -> usize {
        self.upload_count.load(Ordering::SeqCst)
    }

    fn messages_count(&self) -> usize {
        self.messages_count.load(Ordering::SeqCst)
    }

    fn messages_bodies(&self) -> Vec<Value> {
        self.messages_bodies.read().clone()
    }

    fn messages_headers(&self) -> Vec<HashMap<String, String>> {
        self.messages_headers.read().clone()
    }

    fn set_fail_upload(&self, fail: bool) {
        *self.fail_upload.write() = fail;
    }
}

async fn handle_request(
    req: Request<hyper::body::Incoming>,
    state: Arc<MockState>,
) -> anyhow::Result<Response<Full<Bytes>>> {
    let path = req.uri().path().to_string();
    let method = req.method().clone();

    // Collect headers before consuming body
    let headers: HashMap<String, String> = req
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();

    // Read body
    let body_bytes = req.collect().await?.to_bytes();
    let body_str = String::from_utf8_lossy(&body_bytes).to_string();

    // Route: /files (Anthropic Files API upload)
    if path == "/files" && method == Method::POST {
        state.upload_count.fetch_add(1, Ordering::SeqCst);

        if *state.fail_upload.read() {
            return Ok(Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Full::new(Bytes::from("Upload failed")))
                .unwrap());
        }

        // Parse multipart to extract mime type (simplified - we know the test sends PNG)
        let file_id = format!("file_{}", random_id());

        let response_json = json!({
            "id": file_id,
            "type": "file",
            "filename": "test.png",
            "mime_type": "image/png",
            "size_bytes": body_bytes.len() as u64,
            "created_at": Utc::now().to_rfc3339()
        });

        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/json")
            .body(Full::new(Bytes::from(serde_json::to_vec(&response_json)?)))
            .unwrap());
    }

    // Route: /messages (Claude messages)
    if path == "/messages" && method == Method::POST {
        state.messages_count.fetch_add(1, Ordering::SeqCst);

        // Capture headers (especially anthropic-beta)
        state.messages_headers.write().push(headers);

        if let Ok(body_value) = serde_json::from_str::<Value>(&body_str) {
            state.messages_bodies.write().push(body_value);
        }

        // Minimal valid Claude response
        let response_json = json!({
            "id": "msg_test123",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "OK"}],
            "model": "claude-3-5-sonnet-20241022",
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });

        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/json")
            .body(Full::new(Bytes::from(serde_json::to_vec(&response_json)?)))
            .unwrap());
    }

    Ok(Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Full::new(Bytes::from("Not found")))
        .unwrap())
}

async fn run_mock_server(state: Arc<MockState>, listener: TcpListener) {
    loop {
        let (stream, _addr) = match listener.accept().await {
            Ok(conn) => conn,
            Err(_) => continue,
        };

        let io = TokioIo::new(stream);
        let state = state.clone();

        tokio::spawn(async move {
            let _ = http1::Builder::new()
                .serve_connection(
                    io,
                    service_fn(move |req| {
                        let state = state.clone();
                        async move { handle_request(req, state).await }
                    }),
                )
                .await;
        });
    }
}

async fn start_mock_server() -> (String, Arc<MockState>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Failed to bind mock server");
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://127.0.0.1:{}", addr.port());

    let state = Arc::new(MockState::new());
    *state.base_url.write() = base_url.clone();

    let state_clone = state.clone();
    let handle = tokio::spawn(async move {
        run_mock_server(state_clone, listener).await;
    });

    // Wait for server to be ready
    tokio::time::sleep(Duration::from_millis(10)).await;

    (base_url, state, handle)
}

// ============================================================================
// Test Helpers
// ============================================================================

fn make_claude_client(client_name: &str, api_base: &str) -> ClaudeClient {
    let config = ClaudeConfig {
        name: client_name.into(),
        api_key: Some("test-api-key".into()),
        api_base: Some(api_base.into()),
        models: vec![],
        patches: None,
        extra: None,
        system_prompt_prefix: None,
        package: None,
    };

    let mut model = Model::new("claude", "claude-3-5-sonnet-20241022");
    model.data_mut().supports_vision = true;

    ClaudeClient::from_config_for_test(config, model)
}

fn make_bedrock_client(client_name: &str) -> BedrockClient {
    use harnx_core::provider_config::bedrock::BedrockConfig;

    let config = BedrockConfig {
        name: client_name.into(),
        access_key_id: None,
        secret_access_key: None,
        region: None,
        session_token: None,
        profile: None,
        models: vec![],
        patches: None,
        extra: None,
        system_prompt_prefix: None,
        package: None,
    };

    let mut model = Model::new("bedrock", "anthropic.claude-3-sonnet-20240229-v1:0");
    model.data_mut().supports_vision = true;

    BedrockClient::from_config_for_test(config, model)
}

fn make_message_with_cid(cid: &str) -> Message {
    Message {
        role: MessageRole::User,
        content: MessageContent::Array(vec![MessageContentPart::ImageUrl {
            image_url: ImageUrl {
                url: format!("{}{}", CID_PREFIX, cid),
            },
        }]),
        ..Default::default()
    }
}

fn make_attachment_dir(cid: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join(format!("{}.png", cid));
    std::fs::write(&path, b"PNG\x89fake-image-data-12345").unwrap();
    tmp
}

fn make_chat_data(
    messages: Vec<Message>,
    attachments_dir: Option<std::path::PathBuf>,
) -> ChatCompletionsData {
    ChatCompletionsData {
        messages,
        temperature: None,
        top_p: None,
        functions: None,
        stream: false,
        attachments_dir,
    }
}

fn random_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!("{:016x}", COUNTER.fetch_add(1, Ordering::SeqCst))
}

// ============================================================================
// TEST 1: Upload-once-reuse (HEADLINE)
// ============================================================================

/// Test that a cached file reference is reused across multiple turns with ClaudeClient.
///
/// 1. Turn 1: Reference a cid-image → upload to /files → cache populated with file_id
/// 2. Turn 2: Reference the SAME cid-image → cache hit → NO re-upload
///
/// Assertions:
/// - upload_count == 1 (uploaded once)
/// - messages_count == 2 (two turns)
/// - BOTH /messages bodies contain source.type=="file" with source.file_id matching mock response
/// - BOTH /messages requests have anthropic-beta: files-api-2025-04-14 header
#[tokio::test]
async fn upload_once_reuse_across_turns() -> Result<()> {
    let (api_base, state, _server_handle) = start_mock_server().await;

    let client_name = format!("test-claude-upload-once-{}", random_id());
    let client = make_claude_client(&client_name, &api_base);
    let tmp = make_attachment_dir("testimg");

    let reqwest_client = reqwest::Client::new();

    // === TURN 1 ===
    let messages_turn1 = vec![make_message_with_cid("testimg")];
    let data_turn1 = make_chat_data(messages_turn1, Some(tmp.path().to_path_buf()));

    let _ = client
        .chat_completions_inner(&reqwest_client, data_turn1)
        .await;

    tokio::time::sleep(Duration::from_millis(50)).await;

    // === TURN 2 ===
    let messages_turn2 = vec![make_message_with_cid("testimg")];
    let data_turn2 = make_chat_data(messages_turn2, Some(tmp.path().to_path_buf()));

    let _ = client
        .chat_completions_inner(&reqwest_client, data_turn2)
        .await;

    tokio::time::sleep(Duration::from_millis(50)).await;

    // === ASSERTIONS ===
    assert_eq!(
        state.upload_count(),
        1,
        "Expected exactly 1 upload across both turns, got {}",
        state.upload_count()
    );

    assert_eq!(
        state.messages_count(),
        2,
        "Expected 2 /messages calls (one per turn), got {}",
        state.messages_count()
    );

    // Verify both request bodies have source.type=="file" with correct file_id
    let bodies = state.messages_bodies();
    assert_eq!(bodies.len(), 2, "Should have 2 captured bodies");

    let headers = state.messages_headers();
    assert_eq!(headers.len(), 2, "Should have 2 captured header sets");

    for (i, (body, hdrs)) in bodies.iter().zip(headers.iter()).enumerate() {
        // Check source.type == "file"
        let messages = body.get("messages").and_then(|m| m.as_array()).unwrap();
        let first_message = messages.first().unwrap();
        let content = first_message
            .get("content")
            .and_then(|c| c.as_array())
            .unwrap();
        let first_part = content.first().unwrap();
        let source = first_part.get("source").unwrap();
        let source_type = source.get("type").and_then(|t| t.as_str()).unwrap();
        assert_eq!(
            source_type,
            "file",
            "Turn {}: expected source.type=='file', got '{}'",
            i + 1,
            source_type
        );

        // Check source.file_id starts with "file_"
        let file_id = source.get("file_id").and_then(|f| f.as_str()).unwrap();
        assert!(
            file_id.starts_with("file_"),
            "Turn {}: file_id should start with 'file_', got '{}'",
            i + 1,
            file_id
        );

        // Check anthropic-beta header is present
        let beta_header = hdrs.get("anthropic-beta");
        assert!(
            beta_header.is_some(),
            "Turn {}: missing anthropic-beta header",
            i + 1
        );
        assert_eq!(
            beta_header.unwrap(),
            "files-api-2025-04-14",
            "Turn {}: wrong anthropic-beta header value",
            i + 1
        );
    }

    drop(_server_handle);
    Ok(())
}

// ============================================================================
// TEST 2: Fallback on upload failure
// ============================================================================

/// Test that upload failure (5xx on /files) falls back to base64 inline source.
///
/// - Mock /files returns 500
/// - Request still succeeds with base64 source
/// - /messages body uses source.type=="base64" (NOT file)
/// - anthropic-beta header is NOT required/added (no file_ source)
#[tokio::test]
async fn fallback_on_upload_failure() -> Result<()> {
    let (api_base, state, _server_handle) = start_mock_server().await;

    state.set_fail_upload(true);

    let client_name = format!("test-claude-fallback-{}", random_id());
    let client = make_claude_client(&client_name, &api_base);
    let tmp = make_attachment_dir("testimg");

    let reqwest_client = reqwest::Client::new();

    let messages = vec![make_message_with_cid("testimg")];
    let data = make_chat_data(messages, Some(tmp.path().to_path_buf()));

    let _ = client.chat_completions_inner(&reqwest_client, data).await;

    tokio::time::sleep(Duration::from_millis(50)).await;

    // === ASSERTIONS ===
    assert_eq!(
        state.upload_count(),
        1,
        "Upload should have been attempted once"
    );

    assert_eq!(
        state.messages_count(),
        1,
        "Messages endpoint should have been called"
    );

    // Verify source.type == "base64" (fallback)
    let bodies = state.messages_bodies();
    let body = bodies.first().unwrap();
    let messages = body.get("messages").and_then(|m| m.as_array()).unwrap();
    let first_message = messages.first().unwrap();
    let content = first_message
        .get("content")
        .and_then(|c| c.as_array())
        .unwrap();
    let first_part = content.first().unwrap();
    let source = first_part.get("source").unwrap();
    let source_type = source.get("type").and_then(|t| t.as_str()).unwrap();
    assert_eq!(
        source_type, "base64",
        "Expected source.type=='base64' on upload failure, got '{}'",
        source_type
    );

    // Verify anthropic-beta header is NOT present (no file_ refs)
    let headers = state.messages_headers();
    let hdrs = headers.first().unwrap();
    let beta_header = hdrs.get("anthropic-beta");
    assert!(
        beta_header.is_none() || !beta_header.unwrap().contains("files-api"),
        "anthropic-beta header should NOT be present when only base64 sources used"
    );

    drop(_server_handle);
    Ok(())
}

// ============================================================================
// TEST 3: Bedrock forces base64
// ============================================================================

/// Test that BedrockClient never uploads to Files API.
///
/// BedrockClient capability is false (expands_attachments_internally returns false).
/// This guarantees:
/// - No /files upload calls (counter 0)
/// - Base64 path used (inlineData/data: URL)
#[tokio::test]
async fn bedrock_forces_base64() -> Result<()> {
    // This test verifies BedrockClient capability is false.
    // Since Bedrock images are base64-inlined by the runtime pre-pass,
    // we test that claude_build_chat_completions_body handles base64 sources correctly
    // and doesn't depend on uploads/file_id.

    // BedrockClient capability is false - no Files API access
    let bedrock_client = make_bedrock_client(&format!("test-bedrock-{}", random_id()));
    assert!(
        !bedrock_client.expands_attachments_internally(),
        "BedrockClient.expands_attachments_internally() must return false"
    );

    // Create a message with a pre-inlined data: URL
    // (Bedrock images are base64-inlined by the runtime pre-pass)
    let base64_data = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        b"fake-image-data",
    );
    let data_url = format!("data:image/png;base64,{}", base64_data);

    let messages = vec![Message {
        role: MessageRole::User,
        content: MessageContent::Array(vec![MessageContentPart::ImageUrl {
            image_url: ImageUrl { url: data_url },
        }]),
        ..Default::default()
    }];

    let mut model = Model::new("bedrock", "anthropic.claude-3-sonnet-20240229-v1:0");
    model.data_mut().supports_vision = true;

    // No attachments_dir since runtime pre-pass handles base64
    let data = make_chat_data(messages, None);

    // Build body directly without going through the mock server
    // (Bedrock never uploads, so no mock needed for upload path)
    let body = claude_build_chat_completions_body(data, &model, &HashMap::new())?;

    // Verify: request body uses base64 source (no file sources)
    let messages = body.get("messages").and_then(|m| m.as_array()).unwrap();
    let first_message = messages.first().unwrap();
    let content = first_message
        .get("content")
        .and_then(|c| c.as_array())
        .unwrap();

    // Check that no file sources exist
    let has_file_source = content.iter().any(|p| {
        p.get("source")
            .and_then(|s| s.get("type"))
            .and_then(|t| t.as_str())
            .map(|t| t == "file")
            .unwrap_or(false)
    });
    assert!(
        !has_file_source,
        "Bedrock should NOT emit file sources (no Files API)"
    );

    // Check that base64 source is used
    let first_part = content.first().unwrap();
    let source = first_part.get("source");
    assert!(source.is_some(), "Bedrock should emit a source for images");
    let src = source.unwrap();
    let source_type = src.get("type").and_then(|t| t.as_str()).unwrap();
    assert_eq!(
        source_type, "base64",
        "Bedrock should use base64 source type"
    );

    Ok(())
}

// ============================================================================
// TEST 4: Cache expiry notes (Anthropic files don't expire)
// ============================================================================

/// Test that Anthropic file refs have expires_at = None.
///
/// Anthropic Files API doesn't have expiration — files are long-lived.
/// This test verifies the cache stores None for expires_at.
#[tokio::test]
async fn anthropic_file_refs_have_no_expiry() -> Result<()> {
    let (api_base, _state, _server_handle) = start_mock_server().await;

    let client_name = format!("test-claude-no-expiry-{}", random_id());
    let client = make_claude_client(&client_name, &api_base);
    let tmp = make_attachment_dir("testimg");

    let reqwest_client = reqwest::Client::new();

    let messages = vec![make_message_with_cid("testimg")];
    let data = make_chat_data(messages, Some(tmp.path().to_path_buf()));

    let _ = client.chat_completions_inner(&reqwest_client, data).await;

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Verify cache entry has expires_at = None
    let cache = shared_attachment_cache(&client_name);
    let cid = format!("{}testimg", CID_PREFIX);
    let cached = cache.get_valid(&cid, Utc::now());

    assert!(
        cached.is_some(),
        "Cache should have entry for cid after upload"
    );
    let cached = cached.unwrap();
    assert!(
        cached.expires_at.is_none(),
        "Anthropic file refs should have expires_at = None (long-lived)"
    );
    assert!(
        cached.ref_id.starts_with("file_"),
        "Cached ref_id should start with 'file_'"
    );

    drop(_server_handle);
    Ok(())
}

/// Test that a pre-seeded cache entry is reused without re-upload.
///
/// This is supplementary to the main upload-once-reuse test.
/// It proves the cache-hit path by pre-seeding rather than relying on turn 1 upload.
#[tokio::test]
async fn preseeded_cache_hit_skips_upload() -> Result<()> {
    let (api_base, state, _server_handle) = start_mock_server().await;

    let client_name = format!("test-claude-preseed-{}", random_id());

    // Pre-seed the cache with a valid entry (no expiry)
    let cache = shared_attachment_cache(&client_name);
    let cid = format!("{}preseeded", CID_PREFIX);
    let file_id = "file_preseeded123".to_string();
    cache.insert(
        cid.clone(),
        CachedRef {
            ref_id: file_id.clone(),
            mime_type: "image/png".into(),
            expires_at: None, // Anthropic: no expiry
        },
    );

    let client = make_claude_client(&client_name, &api_base);
    let tmp = make_attachment_dir("preseeded");

    let reqwest_client = reqwest::Client::new();

    let messages = vec![make_message_with_cid("preseeded")];
    let data = make_chat_data(messages, Some(tmp.path().to_path_buf()));

    let _ = client.chat_completions_inner(&reqwest_client, data).await;

    tokio::time::sleep(Duration::from_millis(50)).await;

    // === ASSERTIONS ===
    assert_eq!(
        state.upload_count(),
        0,
        "Pre-seeded cache should skip upload entirely"
    );

    assert_eq!(
        state.messages_count(),
        1,
        "Messages endpoint should have been called"
    );

    // Verify body uses the cached file_id
    let bodies = state.messages_bodies();
    let body = bodies.first().unwrap();
    let messages = body.get("messages").and_then(|m| m.as_array()).unwrap();
    let first_message = messages.first().unwrap();
    let content = first_message
        .get("content")
        .and_then(|c| c.as_array())
        .unwrap();
    let first_part = content.first().unwrap();
    let source = first_part.get("source").unwrap();
    let source_file_id = source.get("file_id").and_then(|f| f.as_str()).unwrap();
    assert_eq!(
        source_file_id, "file_preseeded123",
        "Should use cached file_id"
    );

    drop(_server_handle);
    Ok(())
}

//! Integration tests for Gemini upload-by-reference behavior.
//!
//! Uses a network-free in-test mock HTTP server to prove:
//! 1. Upload-once-reuse (real across-turns test — upload once, reuse in turn 2)
//! 2. Expiry-triggered re-upload
//! 3. Fallback to inlineData on upload failure
//! 4. VertexAI never uploads (capability false)
//!
//! All tests use unique client names for cache isolation.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use harnx_client::client::{ChatCompletionsData, Client};
use harnx_client::vertexai::gemini_build_chat_completions_body;
use harnx_client::{GeminiClient, Model, VertexAIClient};
use harnx_core::attachments::{shared_attachment_cache, CachedRef, CID_PREFIX};
use harnx_core::message::{ImageUrl, Message, MessageContent, MessageContentPart, MessageRole};
use harnx_core::provider_config::gemini::GeminiConfig;
use harnx_core::provider_config::vertexai::VertexAIConfig;
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
///
/// Uses Arc for sharing across connection handlers and AtomicUsize/RwLock for
/// thread-safe counters and captured data.
#[derive(Debug)]
struct MockState {
    /// Base URL of the mock server (e.g., "http://127.0.0.1:12345")
    /// Used to construct correct finalize URLs in the mock response.
    base_url: RwLock<String>,
    upload_start_count: AtomicUsize,
    upload_finalize_count: AtomicUsize,
    generate_content_count: AtomicUsize,
    generate_content_bodies: RwLock<Vec<Value>>,
    expiration_time: RwLock<Option<DateTime<Utc>>>,
    fail_upload_start: RwLock<bool>,
    fail_upload_finalize: RwLock<bool>,
}

impl MockState {
    fn new(base_url: String) -> Arc<Self> {
        Arc::new(Self {
            base_url: RwLock::new(base_url),
            upload_start_count: AtomicUsize::new(0),
            upload_finalize_count: AtomicUsize::new(0),
            generate_content_count: AtomicUsize::new(0),
            generate_content_bodies: RwLock::new(Vec::new()),
            expiration_time: RwLock::new(None),
            fail_upload_start: RwLock::new(false),
            fail_upload_finalize: RwLock::new(false),
        })
    }

    fn set_expiration_time(&self, expiry: Option<DateTime<Utc>>) {
        *self.expiration_time.write() = expiry;
    }

    fn set_fail_upload_start(&self, fail: bool) {
        *self.fail_upload_start.write() = fail;
    }

    #[allow(dead_code)]
    fn set_fail_upload_finalize(&self, fail: bool) {
        *self.fail_upload_finalize.write() = fail;
    }

    fn upload_start_count(&self) -> usize {
        self.upload_start_count.load(Ordering::SeqCst)
    }

    fn upload_finalize_count(&self) -> usize {
        self.upload_finalize_count.load(Ordering::SeqCst)
    }

    fn generate_content_count(&self) -> usize {
        self.generate_content_count.load(Ordering::SeqCst)
    }

    fn generate_content_bodies(&self) -> Vec<Value> {
        self.generate_content_bodies.read().clone()
    }
}

/// Handle a single HTTP request to the mock server.
///
/// Routes based on path and the `x-goog-upload-command` header:
/// - START: `x-goog-upload-command: start` — Returns `x-goog-upload-url` for finalize
/// - FINALIZE: `x-goog-upload-command: upload, finalize` — Returns file metadata
/// - generateContent: Returns mock AI response
async fn handle_request(
    req: Request<hyper::body::Incoming>,
    state: Arc<MockState>,
) -> Result<Response<Full<Bytes>>> {
    let path = req.uri().path().to_string();
    let method = req.method().clone();

    // Get upload command header BEFORE consuming body
    let upload_cmd = req
        .headers()
        .get("x-goog-upload-command")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let body_bytes = req.into_body().collect().await?.to_bytes();
    let body_str = String::from_utf8_lossy(&body_bytes);

    // Route requests
    if path.contains("/upload/") && method == Method::POST {
        // Distinguish START vs FINALIZE by x-goog-upload-command header:
        // - START: header == "start"
        // - FINALIZE: header contains "finalize"
        let is_start = upload_cmd == "start";

        if is_start {
            state.upload_start_count.fetch_add(1, Ordering::SeqCst);

            if *state.fail_upload_start.read() {
                return Ok(Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Full::new(Bytes::from("Upload start failed")))
                    .unwrap());
            }

            // Build finalize URL using the KNOWN mock base URL from state
            // (NOT from req.uri().host()/port_u16() which are None for origin-form requests)
            let base_url = state.base_url.read().clone();
            let finalize_url = format!("{}/upload/finalize/{}", base_url, random_id());

            return Ok(Response::builder()
                .status(StatusCode::OK)
                .header("x-goog-upload-url", finalize_url)
                .body(Full::new(Bytes::new()))
                .unwrap());
        } else {
            // FINALIZE (header contains "finalize")
            state.upload_finalize_count.fetch_add(1, Ordering::SeqCst);

            if *state.fail_upload_finalize.read() {
                return Ok(Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Full::new(Bytes::from("Upload finalize failed")))
                    .unwrap());
            }

            // Build file URI using the KNOWN mock base URL
            let base_url = state.base_url.read().clone();
            let file_id = format!("files/{}", random_id());
            let file_uri = format!("{}/{}", base_url, file_id);

            let expiry = *state.expiration_time.read();
            let expiry_str = expiry.map(|t| t.to_rfc3339());

            let response_json = json!({
                "file": {
                    "uri": file_uri,
                    "mimeType": "image/png",
                    "name": file_id,
                    "state": "ACTIVE",
                    "expirationTime": expiry_str
                }
            });

            return Ok(Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", "application/json")
                .body(Full::new(Bytes::from(serde_json::to_vec(&response_json)?)))
                .unwrap());
        }
    } else if path.contains(":generateContent") {
        state.generate_content_count.fetch_add(1, Ordering::SeqCst);

        if let Ok(body_value) = serde_json::from_str::<Value>(&body_str) {
            state.generate_content_bodies.write().push(body_value);
        }

        let response_json = json!({
            "candidates": [{
                "content": {
                    "parts": [{"text": "OK"}],
                    "role": "model"
                },
                "finishReason": "STOP"
            }]
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

        // Spawn per-connection handler (required for connection draining)
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

    let state = MockState::new(base_url.clone());
    let server_state = state.clone();

    let handle = tokio::spawn(async move {
        run_mock_server(server_state, listener).await;
    });

    tokio::time::sleep(Duration::from_millis(10)).await;

    (base_url, state, handle)
}

// ============================================================================
// Helper Functions
// ============================================================================

fn make_gemini_client(name: &str, api_base: &str) -> GeminiClient {
    let mut model = Model::new("gemini", "gemini-2.0-flash-exp");
    model.data_mut().supports_vision = true;

    let config = GeminiConfig {
        name: name.into(),
        api_key: Some("test-api-key".into()),
        api_base: Some(api_base.into()),
        models: vec![],
        extra: None,
        patches: None,
        system_prompt_prefix: None,
        package: None,
    };

    GeminiClient::from_config_for_test(config, model)
}

fn make_vertexai_client(name: &str) -> VertexAIClient {
    let mut model = Model::new("vertexai", "gemini-2.0-flash-exp");
    model.data_mut().supports_vision = true;

    let config = VertexAIConfig {
        name: name.into(),
        project_id: Some("test-project".into()),
        location: Some("us-central1".into()),
        adc_file: None,
        models: vec![],
        extra: None,
        patches: None,
        system_prompt_prefix: None,
        package: None,
    };

    VertexAIClient::from_config_for_test(config, model)
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

fn make_message_with_data_url(data_url: &str) -> Message {
    Message {
        role: MessageRole::User,
        content: MessageContent::Array(vec![MessageContentPart::ImageUrl {
            image_url: ImageUrl {
                url: data_url.into(),
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
// TEST 1: Upload-once-reuse (HEADLINE — real across-turns test)
// ============================================================================

/// Test that a cached file reference is reused across multiple turns.
///
/// This is the REAL across-turns test (not cache pre-seeding):
/// 1. Turn 1: Reference a cid-image → upload START + FINALIZE → cache populated
/// 2. Turn 2: Reference the SAME cid-image → cache hit → NO re-upload
///
/// Assertions:
/// - upload_start_count == 1 (uploaded once)
/// - upload_finalize_count == 1 (finalize called once)
/// - generate_content_count == 2 (two turns)
/// - BOTH generateContent bodies contain fileData.fileUri matching the mock's returned URI
#[tokio::test]
async fn upload_once_reuse_across_turns() -> Result<()> {
    let (api_base, state, _server_handle) = start_mock_server().await;

    let client_name = format!("test-upload-once-{}", random_id());

    // Set future expiry so the cached entry is valid
    state.set_expiration_time(Some(Utc::now() + ChronoDuration::hours(48)));

    let client = make_gemini_client(&client_name, &api_base);
    let tmp = make_attachment_dir("testimg");

    let reqwest_client = reqwest::Client::new();

    // === TURN 1 ===
    let messages_turn1 = vec![make_message_with_cid("testimg")];
    let data_turn1 = make_chat_data(messages_turn1, Some(tmp.path().to_path_buf()));

    let _ = client
        .chat_completions_inner(&reqwest_client, data_turn1)
        .await;

    // Wait for async upload to complete
    tokio::time::sleep(Duration::from_millis(200)).await;

    // === TURN 2 ===
    // Re-use the SAME client (same cache) and SAME cid
    let messages_turn2 = vec![make_message_with_cid("testimg")];
    let data_turn2 = make_chat_data(messages_turn2, Some(tmp.path().to_path_buf()));

    let _ = client
        .chat_completions_inner(&reqwest_client, data_turn2)
        .await;

    // Wait for any async operations
    tokio::time::sleep(Duration::from_millis(200)).await;

    // === ASSERTIONS ===
    assert_eq!(
        state.upload_start_count(),
        1,
        "Expected exactly 1 upload START across both turns, got {}",
        state.upload_start_count()
    );

    assert_eq!(
        state.upload_finalize_count(),
        1,
        "Expected exactly 1 upload FINALIZE across both turns, got {}",
        state.upload_finalize_count()
    );

    assert_eq!(
        state.generate_content_count(),
        2,
        "Expected 2 generateContent calls (one per turn), got {}",
        state.generate_content_count()
    );

    // Both generateContent bodies should contain fileData with the mock's fileUri
    let bodies = state.generate_content_bodies();
    assert_eq!(bodies.len(), 2, "Should have 2 generateContent bodies");

    // Get the file URI from turn 1's body (same URI should appear in turn 2)
    let body1 = &bodies[0];
    let contents1 = body1.get("contents").and_then(|c| c.as_array()).unwrap();
    let parts1 = contents1
        .first()
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.as_array())
        .unwrap();
    let file_data1 = parts1.iter().find(|p| p.get("fileData").is_some());
    assert!(
        file_data1.is_some(),
        "Turn 1 request should have fileData after successful upload"
    );
    let file_uri = file_data1
        .unwrap()
        .get("fileData")
        .and_then(|fd| fd.get("fileUri"))
        .and_then(|u| u.as_str())
        .expect("fileData should have fileUri")
        .to_string();

    // Verify turn 2 also has fileData with the same fileUri
    let body2 = &bodies[1];
    let contents2 = body2.get("contents").and_then(|c| c.as_array()).unwrap();
    let parts2 = contents2
        .first()
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.as_array())
        .unwrap();
    let file_data2 = parts2.iter().find(|p| p.get("fileData").is_some());
    assert!(
        file_data2.is_some(),
        "Turn 2 request should have fileData (from cache reuse)"
    );
    let file_uri2 = file_data2
        .unwrap()
        .get("fileData")
        .and_then(|fd| fd.get("fileUri"))
        .and_then(|u| u.as_str())
        .expect("fileData should have fileUri")
        .to_string();

    assert_eq!(
        file_uri, file_uri2,
        "Both turns should reference the same fileUri (uploaded once, reused)"
    );

    drop(_server_handle);
    Ok(())
}

// ============================================================================
// TEST 1b: Cache hit (pre-seeded — retained for completeness)
// ============================================================================

/// Test that a cache pre-seeded with a valid entry skips the upload.
#[tokio::test]
async fn upload_cache_hit_skips_upload() -> Result<()> {
    let (api_base, state, _server_handle) = start_mock_server().await;

    let client_name = format!("test-cache-hit-{}", random_id());
    let cache = shared_attachment_cache(&client_name);

    // Pre-seed cache with valid entry
    let cid = "cid:testimg";
    let file_uri = format!("{}/files/cached-file", api_base);
    cache.insert(
        cid.to_string(),
        CachedRef {
            ref_id: file_uri.clone(),
            mime_type: "image/png".into(),
            expires_at: Some(Utc::now() + ChronoDuration::hours(48)),
        },
    );

    let client = make_gemini_client(&client_name, &api_base);
    let tmp = make_attachment_dir("testimg");

    let messages = vec![make_message_with_cid("testimg")];
    let data = make_chat_data(messages, Some(tmp.path().to_path_buf()));

    let reqwest_client = reqwest::Client::new();
    let _ = client.chat_completions_inner(&reqwest_client, data).await;

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Upload should NOT have been called (cache hit)
    assert_eq!(
        state.upload_start_count(),
        0,
        "Expected NO upload START (cache hit), got {}",
        state.upload_start_count()
    );

    // generateContent should have been called
    assert_eq!(
        state.generate_content_count(),
        1,
        "Expected 1 generateContent call, got {}",
        state.generate_content_count()
    );

    // Request should have fileData (from cache)
    let bodies = state.generate_content_bodies();
    assert_eq!(bodies.len(), 1);

    let body = &bodies[0];
    let contents = body.get("contents").and_then(|c| c.as_array()).unwrap();
    let parts = contents
        .first()
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.as_array())
        .unwrap();

    assert!(
        parts.iter().any(|p| p.get("fileData").is_some()),
        "Request should have fileData from cache"
    );

    drop(_server_handle);
    Ok(())
}

// ============================================================================
// TEST 2: Expiry re-upload
// ============================================================================

/// Test that an expired cache entry triggers re-upload.
#[tokio::test]
async fn expiry_reupload() -> Result<()> {
    let (api_base, state, _server_handle) = start_mock_server().await;

    let client_name = format!("test-expiry-reupload-{}", random_id());

    // Pre-seed cache with expired entry
    let cache = shared_attachment_cache(&client_name);
    let cid = format!("{}testimg", CID_PREFIX);
    let expired_time = Utc::now() - ChronoDuration::hours(1);
    cache.insert(
        cid.clone(),
        CachedRef {
            ref_id: "http://expired.example/files/old".into(),
            mime_type: "image/png".into(),
            expires_at: Some(expired_time),
        },
    );

    state.set_expiration_time(Some(Utc::now() + ChronoDuration::hours(48)));

    let client = make_gemini_client(&client_name, &api_base);
    let tmp = make_attachment_dir("testimg");

    let messages = vec![make_message_with_cid("testimg")];
    let data = make_chat_data(messages, Some(tmp.path().to_path_buf()));

    let reqwest_client = reqwest::Client::new();
    let _ = client.chat_completions_inner(&reqwest_client, data).await;

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Upload should have been called (expired entry)
    assert_eq!(
        state.upload_start_count(),
        1,
        "Expected 1 upload START (expired entry triggers re-upload), got {}",
        state.upload_start_count()
    );

    drop(_server_handle);
    Ok(())
}

// ============================================================================
// TEST 3: Fallback on upload failure
// ============================================================================

/// Test that upload finalize failure falls back to inlineData (base64).
#[tokio::test]
async fn fallback_on_upload_failure() -> Result<()> {
    let (api_base, state, _server_handle) = start_mock_server().await;

    state.set_fail_upload_start(true);

    let client_name = format!("test-failure-{}", random_id());
    let client = make_gemini_client(&client_name, &api_base);

    let tmp = make_attachment_dir("testimg");

    let messages = vec![make_message_with_cid("testimg")];
    let data = make_chat_data(messages, Some(tmp.path().to_path_buf()));

    let reqwest_client = reqwest::Client::new();
    let _ = client.chat_completions_inner(&reqwest_client, data).await;

    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(
        state.upload_start_count(),
        1,
        "Expected upload START attempt"
    );
    assert_eq!(
        state.upload_finalize_count(),
        0,
        "FINALIZE should not be called when START fails"
    );

    let bodies = state.generate_content_bodies();
    assert_eq!(bodies.len(), 1);

    let body = &bodies[0];
    let contents = body.get("contents").and_then(|c| c.as_array()).unwrap();
    let parts = contents
        .first()
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.as_array())
        .unwrap();

    assert!(
        parts.iter().any(|p| p.get("inlineData").is_some()),
        "Should have inlineData fallback"
    );
    assert!(
        !parts.iter().any(|p| p.get("fileData").is_some()),
        "Should NOT have fileData"
    );

    drop(_server_handle);
    Ok(())
}

// ============================================================================
// TEST 4: Vertex forces base64 (no uploads)
// ============================================================================

/// Test that VertexAI never uploads to Files API.
#[tokio::test]
async fn vertex_forces_base64() -> Result<()> {
    let (_api_base, state, _server_handle) = start_mock_server().await;

    let client_name = format!("test-vertex-base64-{}", random_id());

    let vertex_client = make_vertexai_client(&client_name);

    // KEY ASSERTION: VertexAI capability is false
    assert!(
        !vertex_client.expands_attachments_internally(),
        "VertexAIClient.expands_attachments_internally() must return false"
    );

    // Create a message with a pre-inlined data: URL
    let base64_data = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        b"fake-image-data",
    );
    let data_url = format!("data:image/png;base64,{}", base64_data);

    let messages = vec![make_message_with_data_url(&data_url)];

    let mut model = Model::new("vertexai", "gemini-2.0-flash-exp");
    model.data_mut().supports_vision = true;

    let data = make_chat_data(messages, None);

    let body = gemini_build_chat_completions_body(data, &model, HashMap::new())?;

    // Verify: request body has inlineData, NOT fileData
    let contents = body.get("contents").and_then(|c| c.as_array()).unwrap();
    let parts = contents
        .first()
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.as_array())
        .unwrap();

    let has_inline_data = parts.iter().any(|p| p.get("inlineData").is_some());
    let has_file_data = parts.iter().any(|p| p.get("fileData").is_some());

    assert!(
        has_inline_data,
        "VertexAI request should have inlineData for data: URLs"
    );
    assert!(
        !has_file_data,
        "VertexAI request should NOT have fileData (no Files API)"
    );

    // Verify: no upload calls were made
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(
        state.upload_start_count(),
        0,
        "VertexAI should NOT trigger upload START (capability=false guarantees no Files API)"
    );

    drop(_server_handle);
    Ok(())
}

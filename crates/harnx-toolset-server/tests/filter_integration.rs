mod common;

use anyhow::{Context, Result};
use common::{request_headers, spawn_nats_server, wait_for_registration, TestToolset, TOKEN};
use globset::GlobSet;
use harnx_core::instance::ServerScope;
use harnx_nats_common::connect::NatsConnection;
use harnx_toolset::{ToolReply, ToolRequest};
use harnx_toolset_server::invocation_journal::InvocationJournal;
use harnx_toolset_server::{compile_enable_globs, serve_with_config, ServeConfig, ServeLifecycle};
use serde_json::json;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// A filtered test harness that spawns nats-server and serves a FilteredToolset.
struct FilteredHarness {
    _server: common::NatsServerHandle,
    server_task: Option<tokio::task::JoinHandle<Result<()>>>,
    shutdown: CancellationToken,
    client: async_nats::Client,
    instance_id: ServerScope,
    toolset: TestToolset,
}

async fn setup_test_context(patterns: &[&str]) -> Result<Option<FilteredHarness>> {
    let patterns = patterns
        .iter()
        .map(|pattern| (*pattern).to_string())
        .collect::<Vec<_>>();
    let filter = compile_enable_globs(&patterns)?.map(Arc::new);
    FilteredHarness::start(filter).await
}

impl FilteredHarness {
    async fn start(filter: Option<Arc<GlobSet>>) -> Result<Option<Self>> {
        let toolset = TestToolset::default();
        Self::with_toolset(toolset, filter).await
    }

    async fn with_toolset(
        toolset: TestToolset,
        filter: Option<Arc<GlobSet>>,
    ) -> Result<Option<Self>> {
        let Some(server) = spawn_nats_server().await? else {
            return Ok(None);
        };
        let instance_id = ServerScope::new();
        let shutdown = CancellationToken::new();
        let readiness = harnx_healthz::Readiness::default();
        let server_client = async_nats::ConnectOptions::new()
            .token(TOKEN.to_string())
            .connect(&server.url)
            .await?;

        // Wrap the toolset with FilteredToolset if filter is provided
        let server_toolset: Arc<dyn harnx_toolset::Toolset> = match &filter {
            Some(filter_set) => Arc::new(harnx_toolset_server::FilteredToolset::new(
                toolset.clone(),
                (**filter_set).clone(),
            )),
            None => Arc::new(toolset.clone()),
        };

        let server_instance_id = instance_id.clone();
        let server_shutdown = shutdown.clone();
        let server_readiness = readiness.clone();
        let server_filter = filter.clone();
        let server_task = tokio::spawn(async move {
            serve_with_config(
                server_toolset,
                ServeConfig {
                    instance_id: server_instance_id,
                    connection: NatsConnection {
                        client: server_client,
                        replicas: 1,
                    },
                    lifecycle: ServeLifecycle::new(server_shutdown, Some(server_readiness)),
                    filter: server_filter,
                },
            )
            .await
        });
        let client = async_nats::ConnectOptions::new()
            .token(TOKEN.to_string())
            .connect(&server.url)
            .await?;
        Ok(Some(Self {
            _server: server,
            server_task: Some(server_task),
            shutdown,
            client,
            instance_id,
            toolset,
        }))
    }

    fn tool_subject(&self, tool: &str) -> String {
        self.instance_id.tool_subject("____test", tool)
    }

    async fn shutdown(&mut self) {
        self.shutdown.cancel();
        if let Some(server_task) = self.server_task.take() {
            let _ = server_task.await;
        }
    }
}

// Test 1: Registration filtering
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registration_filters_tools_by_glob() -> Result<()> {
    let mut harness = setup_test_context(&["echo"])
        .await?
        .context("nats-server required")?;

    let registration = wait_for_registration(&harness.client, &harness.instance_id).await?;
    let tool_names: Vec<_> = registration.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(tool_names, vec!["echo"]);
    assert!(!tool_names.contains(&"fail"));
    assert!(!tool_names.contains(&"sleep"));

    harness.shutdown().await;
    Ok(())
}

// Test 2: Invocation rejection - disabled tool returns unavailable error
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invocation_rejected_for_disabled_tool() -> Result<()> {
    let mut harness = setup_test_context(&["echo"])
        .await?
        .context("nats-server required")?;

    wait_for_registration(&harness.client, &harness.instance_id).await?;

    let request = ToolRequest {
        replay: None,
        operation_id: "fail-call".to_string(),
        call_id: "fail-call".to_string(),
        tool: "fail".to_string(),
        args: json!({}),
        parent_session_id: Some("session".to_string()),
        tool_call_id: Some("model-call".to_string()),
        capabilities: Default::default(),
    };

    let message = harness
        .client
        .request_with_headers(
            harness.tool_subject("fail"),
            request_headers(&request.call_id, "idempotency-fail"),
            serde_json::to_vec(&request)?.into(),
        )
        .await?;

    let reply: ToolReply = serde_json::from_slice(&message.payload)?;
    assert!(reply.result.is_err());
    let error = reply.result.unwrap_err();
    assert!(
        matches!(error, harnx_toolset::ToolErrorPayload::Recoverable(msg) if msg.contains("is not available on this server"))
    );

    // Tool invocation counter should NOT have incremented
    assert_eq!(
        harness
            .toolset
            .fail_invocations
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );

    harness.shutdown().await;
    Ok(())
}

// Test 3: Cache bypass regression - idempotent cache does not bypass admission gate
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idempotent_cache_does_not_bypass_admission_gate() -> Result<()> {
    let mut harness = setup_test_context(&["echo"])
        .await?
        .context("nats-server required")?;

    wait_for_registration(&harness.client, &harness.instance_id).await?;

    let request = ToolRequest {
        replay: None,
        operation_id: "fail-call".to_string(),
        call_id: "fail-call".to_string(),
        tool: "fail".to_string(),
        args: json!({}),
        parent_session_id: Some("session".to_string()),
        tool_call_id: Some("model-call".to_string()),
        capabilities: Default::default(),
    };

    // Send the same request twice with identical idempotency key
    for _ in 0..2 {
        let message = harness
            .client
            .request_with_headers(
                harness.tool_subject("fail"),
                request_headers(&request.call_id, "idempotency-fail-duplicate"),
                serde_json::to_vec(&request)?.into(),
            )
            .await?;

        let reply: ToolReply = serde_json::from_slice(&message.payload)?;
        assert!(reply.result.is_err());
        let error = reply.result.unwrap_err();
        assert!(
            matches!(error, harnx_toolset::ToolErrorPayload::Recoverable(msg) if msg.contains("is not available on this server"))
        );
    }

    // Both requests should be rejected by admission gate, tool never executes
    assert_eq!(
        harness
            .toolset
            .fail_invocations
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );

    harness.shutdown().await;
    Ok(())
}

// Test 4: Journal bypass regression - seeded journal reply is NOT returned for disabled tool
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn journal_reply_not_returned_for_disabled_tool() -> Result<()> {
    let mut harness = setup_test_context(&["echo"])
        .await?
        .context("nats-server required")?;

    wait_for_registration(&harness.client, &harness.instance_id).await?;

    let js = async_nats::jetstream::new(harness.client.clone());
    let journal = InvocationJournal::ensure(&js, 1).await?;

    // Pre-seed the journal with a completed reply for a disabled tool
    let request = ToolRequest {
        replay: None,
        operation_id: "fail-call".to_string(),
        call_id: "fail-call".to_string(),
        tool: "fail".to_string(),
        args: json!({}),
        parent_session_id: Some("session".to_string()),
        tool_call_id: Some("model-call".to_string()),
        capabilities: Default::default(),
    };

    journal
        .record(&request, ("test_fail", "test-scope", "____test"), 1)
        .await?;

    let saved_reply = ToolReply {
        call_id: request.call_id.clone(),
        result: Ok(json!({"privileged": "data"})),
    };
    journal.complete(&request, saved_reply.clone()).await?;

    // Send request for the disabled tool - admission gate should reject BEFORE returning journal reply
    let message = harness
        .client
        .request_with_headers(
            harness.tool_subject("fail"),
            request_headers(&request.call_id, "idempotency-fail-journal"),
            serde_json::to_vec(&request)?.into(),
        )
        .await?;

    let reply: ToolReply = serde_json::from_slice(&message.payload)?;
    assert!(reply.result.is_err());
    let error = reply.result.clone().unwrap_err();
    assert!(
        matches!(error, harnx_toolset::ToolErrorPayload::Recoverable(msg) if msg.contains("is not available on this server"))
    );

    // The privileged reply should NOT be returned
    assert_ne!(reply.result, Ok(json!({"privileged": "data"})));

    // Tool invocation counter should NOT have incremented
    assert_eq!(
        harness
            .toolset
            .fail_invocations
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );

    harness.shutdown().await;
    Ok(())
}

// Test 5: Sanity check - enabled tool works normally and replay works
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enabled_tool_works_normally_with_durable_replay() -> Result<()> {
    let filter = compile_enable_globs(&["echo".to_string(), "sleep".to_string()])
        .unwrap()
        .map(Arc::new);
    let mut toolset = TestToolset::default();
    toolset.idempotent = true;
    let mut harness = FilteredHarness::with_toolset(toolset, filter)
        .await?
        .context("nats-server required")?;

    wait_for_registration(&harness.client, &harness.instance_id).await?;

    let request = ToolRequest {
        replay: None,
        operation_id: "echo-call".to_string(),
        call_id: "echo-call".to_string(),
        tool: "echo".to_string(),
        args: json!({"value": 42}),
        parent_session_id: Some("session".to_string()),
        tool_call_id: Some("model-call".to_string()),
        capabilities: Default::default(),
    };

    // First request should succeed
    let message = harness
        .client
        .request_with_headers(
            harness.tool_subject("echo"),
            request_headers(&request.call_id, "idempotency-echo"),
            serde_json::to_vec(&request)?.into(),
        )
        .await?;

    let reply: ToolReply = serde_json::from_slice(&message.payload)?;
    assert!(reply.result.is_ok());
    assert_eq!(reply.result.unwrap(), json!({"value": 42}));

    // Tool should have been invoked once
    let invocations = harness
        .toolset
        .echo_invocations
        .load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(invocations, 1);

    // Replay the same request (with same idempotency key) - should use cache
    let message2 = harness
        .client
        .request_with_headers(
            harness.tool_subject("echo"),
            request_headers(&request.call_id, "idempotency-echo"),
            serde_json::to_vec(&request)?.into(),
        )
        .await?;

    let reply2: ToolReply = serde_json::from_slice(&message2.payload)?;
    assert!(reply2.result.is_ok());
    assert_eq!(reply2.result.unwrap(), json!({"value": 42}));

    // Cache should prevent re-invocation
    let invocations_after = harness
        .toolset
        .echo_invocations
        .load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(invocations_after, 1);

    harness.shutdown().await;
    Ok(())
}

// Additional test: Registration with no filter shows all tools
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registration_shows_all_tools_when_no_filter() -> Result<()> {
    let mut harness = FilteredHarness::start(None)
        .await?
        .context("nats-server required")?;

    let registration = wait_for_registration(&harness.client, &harness.instance_id).await?;
    let tool_names: std::collections::BTreeSet<_> =
        registration.tools.iter().map(|t| t.name.as_str()).collect();
    assert!(tool_names.contains("echo"));
    assert!(tool_names.contains("fail"));
    assert!(tool_names.contains("sleep"));

    harness.shutdown().await;
    Ok(())
}

// Test: Glob pattern filtering for tools list
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registration_filters_by_glob_pattern() -> Result<()> {
    let mut harness = setup_test_context(&["*e*"])
        .await?
        .context("nats-server required")?;

    let registration = wait_for_registration(&harness.client, &harness.instance_id).await?;
    let tool_names: Vec<_> = registration.tools.iter().map(|t| t.name.as_str()).collect();
    // "echo" and "sleep" contain 'e', "fail" does not
    assert!(tool_names.contains(&"echo"));
    assert!(tool_names.contains(&"sleep"));
    assert!(!tool_names.contains(&"fail"));

    harness.shutdown().await;
    Ok(())
}

// Test: Admission gate rejects even for disabled tool name in wildcard subscription
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admission_gate_rejects_disabled_tool_via_wildcard() -> Result<()> {
    let mut harness = setup_test_context(&["echo"])
        .await?
        .context("nats-server required")?;

    wait_for_registration(&harness.client, &harness.instance_id).await?;

    // Send request directly via wildcard subject (bypassing registration lookup)
    let request = ToolRequest {
        replay: None,
        operation_id: "sleep-call".to_string(),
        call_id: "sleep-call".to_string(),
        tool: "sleep".to_string(),
        args: json!({}),
        parent_session_id: Some("session".to_string()),
        tool_call_id: Some("model-call".to_string()),
        capabilities: Default::default(),
    };

    let message = harness
        .client
        .request_with_headers(
            harness.tool_subject("sleep"),
            request_headers(&request.call_id, "idempotency-sleep"),
            serde_json::to_vec(&request)?.into(),
        )
        .await?;

    let reply: ToolReply = serde_json::from_slice(&message.payload)?;
    assert!(reply.result.is_err());
    let error = reply.result.unwrap_err();
    assert!(
        matches!(error, harnx_toolset::ToolErrorPayload::Recoverable(msg) if msg.contains("is not available on this server"))
    );

    // Tool should not have been invoked
    assert_eq!(
        harness
            .toolset
            .sleep_invocations
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );

    harness.shutdown().await;
    Ok(())
}

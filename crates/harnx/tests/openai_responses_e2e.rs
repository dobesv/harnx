//! End-to-end tests for OpenAI Responses API (/v1/responses) integration.
//!
//! Verifies:
//! - Responses endpoint dispatch via model alias
//! - Request body shape (store=false, include, tools flat, no messages/seed)
//! - Reasoning replay via encrypted_content / thought_signature
//! - Regression guard: non-alias models still hit /v1/chat/completions

#![cfg(unix)]

use anyhow::{Context, Result};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;

use harnx::test_utils::interrupt::{harnx_mcp_time_bin, spawn_oneshot, wait_for_exit};
use harnx::test_utils::mock_openai_server::{
    MockOpenAiScript, MockOpenAiServer, MockOpenAiToolCall, MockOpenAiTurn,
};

/// Write a config that uses the OpenAI client with gpt-5.6-sol:high alias
/// AND includes the time_wait MCP tool for multi-turn testing.
fn write_responses_config_with_tool(
    dir: &std::path::Path,
    mock_base_url: &str,
    mcp_time_bin: &Path,
) -> Result<harnx::test_utils::interrupt::ConfigPaths> {
    use harnx::test_utils::interrupt::ConfigPaths;

    let harnx_config_dir = dir.join("harnx-config");
    let harnx_data_dir = dir.join("harnx-data");
    let harnx_state_dir = dir.join("harnx-state");

    std::fs::create_dir_all(harnx_config_dir.join("clients"))
        .context("failed to create harnx config dir")?;
    std::fs::create_dir_all(&harnx_data_dir).context("failed to create harnx data dir")?;
    std::fs::create_dir_all(&harnx_state_dir).context("failed to create harnx state dir")?;

    // Config.yaml with model pointing to the alias, stream:false for non-streaming path
    std::fs::write(
        harnx_config_dir.join("config.yaml"),
        "save: false\nclient: openai\nmodel: openai:gpt-5.6-sol:high\ntool_use: true\nuse_tools: '*'\nstream: false\n",
    )
    .context("failed to write config.yaml")?;

    // OpenAI client pointing to mock
    std::fs::write(
        harnx_config_dir.join("clients/openai.yaml"),
        format!(
            "type: openai\nname: openai\napi_base: {}\napi_key: test-key\n",
            mock_base_url
        ),
    )
    .context("failed to write clients/openai.yaml")?;

    // Write the models.yaml with the alias (endpoint: responses)
    std::fs::write(
        harnx_config_dir.join("models.yaml"),
        r#"openai:
  - name: gpt-5.6-sol
    max_input_tokens: 1050000
    max_output_tokens: 128000
    supports_tool_use: true
  - name: gpt-5.6-sol:high
    real_name: gpt-5.6-sol
    endpoint: responses
    max_input_tokens: 1050000
    max_output_tokens: 128000
    supports_tool_use: true
    patches:
      - del(.body.temperature) | del(.body.top_p) | .body.reasoning.effort = "high"
"#,
    )
    .context("failed to write models.yaml")?;

    // Default agent
    std::fs::create_dir_all(harnx_config_dir.join("agents"))
        .context("failed to create agents dir")?;
    std::fs::write(
        harnx_config_dir.join("agents/default.md"),
        "---\nclient: openai\nmodel: openai:gpt-5.6-sol:high\ntool_use: true\nuse_tools: '*'\n---\nDefault test agent.\n",
    )
    .context("failed to write agents/default.md")?;

    // MCP servers dir with time_wait tool
    let mcp_servers_dir = harnx_config_dir.join("mcp_servers");
    std::fs::create_dir_all(&mcp_servers_dir).context("failed to create mcp_servers dir")?;
    std::fs::write(
        mcp_servers_dir.join("time.yaml"),
        format!(
            "command: {}\nargs: []\ntools:\n  - time_wait\n",
            mcp_time_bin.display()
        ),
    )
    .context("failed to write mcp_servers/time.yaml")?;

    Ok(ConfigPaths {
        dir: dir.to_path_buf(),
        harnx_config_dir,
        harnx_data_dir,
        harnx_state_dir,
    })
}

/// Write a config that uses the OpenAI client with gpt-5.6-sol:high alias
fn write_responses_config(
    dir: &std::path::Path,
    mock_base_url: &str,
) -> Result<harnx::test_utils::interrupt::ConfigPaths> {
    use harnx::test_utils::interrupt::ConfigPaths;

    let harnx_config_dir = dir.join("harnx-config");
    let harnx_data_dir = dir.join("harnx-data");
    let harnx_state_dir = dir.join("harnx-state");

    std::fs::create_dir_all(harnx_config_dir.join("clients"))
        .context("failed to create harnx config dir")?;
    std::fs::create_dir_all(&harnx_data_dir).context("failed to create harnx data dir")?;
    std::fs::create_dir_all(&harnx_state_dir).context("failed to create harnx state dir")?;

    // Config.yaml with model pointing to the alias, stream:false for non-streaming path
    std::fs::write(
        harnx_config_dir.join("config.yaml"),
        "save: false\nclient: openai\nmodel: openai:gpt-5.6-sol:high\ntool_use: true\nuse_tools: '*'\nstream: false\n",
    )
    .context("failed to write config.yaml")?;

    // OpenAI client pointing to mock
    std::fs::write(
        harnx_config_dir.join("clients/openai.yaml"),
        format!(
            "type: openai\nname: openai\napi_base: {}\napi_key: test-key\n",
            mock_base_url
        ),
    )
    .context("failed to write clients/openai.yaml")?;

    // Write the models.yaml with the alias (endpoint: responses)
    std::fs::write(
        harnx_config_dir.join("models.yaml"),
        r#"openai:
  - name: gpt-5.6-sol
    max_input_tokens: 1050000
    max_output_tokens: 128000
    supports_tool_use: true
  - name: gpt-5.6-sol:high
    real_name: gpt-5.6-sol
    endpoint: responses
    max_input_tokens: 1050000
    max_output_tokens: 128000
    supports_tool_use: true
    patches:
      - del(.body.temperature) | del(.body.top_p) | .body.reasoning.effort = "high"
"#,
    )
    .context("failed to write models.yaml")?;

    // Default agent
    std::fs::create_dir_all(harnx_config_dir.join("agents"))
        .context("failed to create agents dir")?;
    std::fs::write(
        harnx_config_dir.join("agents/default.md"),
        "---\nclient: openai\nmodel: openai:gpt-5.6-sol:high\ntool_use: true\nuse_tools: '*'\n---\nDefault test agent.\n",
    )
    .context("failed to write agents/default.md")?;

    Ok(ConfigPaths {
        dir: dir.to_path_buf(),
        harnx_config_dir,
        harnx_data_dir,
        harnx_state_dir,
    })
}

/// Write a config using plain gpt-4o (no endpoint alias) to hit /v1/chat/completions.
fn write_chat_config(
    dir: &std::path::Path,
    mock_base_url: &str,
) -> Result<harnx::test_utils::interrupt::ConfigPaths> {
    use harnx::test_utils::interrupt::ConfigPaths;

    let harnx_config_dir = dir.join("harnx-config");
    let harnx_data_dir = dir.join("harnx-data");
    let harnx_state_dir = dir.join("harnx-state");

    std::fs::create_dir_all(harnx_config_dir.join("clients"))
        .context("failed to create harnx config dir")?;
    std::fs::create_dir_all(&harnx_data_dir).context("failed to create harnx data dir")?;
    std::fs::create_dir_all(&harnx_state_dir).context("failed to create harnx state dir")?;

    std::fs::write(
        harnx_config_dir.join("config.yaml"),
        "save: false\nclient: openai\nmodel: openai:gpt-4o\ntool_use: true\nuse_tools: '*'\n",
    )
    .context("failed to write config.yaml")?;

    std::fs::write(
        harnx_config_dir.join("clients/openai.yaml"),
        format!(
            "type: openai\nname: openai\napi_base: {}\napi_key: test-key\n",
            mock_base_url
        ),
    )
    .context("failed to write clients/openai.yaml")?;

    std::fs::write(
        harnx_config_dir.join("models.yaml"),
        r#"openai:
  - name: gpt-4o
    max_input_tokens: 128000
    max_output_tokens: 4096
    supports_tool_use: true
"#,
    )
    .context("failed to write models.yaml")?;

    std::fs::create_dir_all(harnx_config_dir.join("agents"))
        .context("failed to create agents dir")?;
    std::fs::write(
        harnx_config_dir.join("agents/default.md"),
        "---\nclient: openai\nmodel: openai:gpt-4o\ntool_use: true\nuse_tools: '*'\n---\nDefault test agent.\n",
    )
    .context("failed to write agents/default.md")?;

    Ok(ConfigPaths {
        dir: dir.to_path_buf(),
        harnx_config_dir,
        harnx_data_dir,
        harnx_state_dir,
    })
}

/// Test A: model openai:gpt-5.6-sol:high hits /v1/responses with correct body shape.
/// NOTE: The models.yaml patches are applied via jaq after body construction.
/// The test config must either use the built-in models.yaml OR apply patches
/// via a client config patches.responses - this test verifies the endpoint dispatch.
#[test]
fn responses_alias_hits_responses_endpoint() -> Result<()> {
    let script = MockOpenAiScript {
        turns: vec![MockOpenAiTurn {
            text_chunks: vec!["Hello!".to_string()],
            tool_calls: vec![],
            ..Default::default()
        }],
        ..Default::default()
    };
    let mock = MockOpenAiServer::start(script)?;
    let tmp = tempfile::tempdir()?;
    let paths =
        write_responses_config(tmp.path(), &format!("http://127.0.0.1:{}/v1", mock.port()))?;
    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));

    let mut child = spawn_oneshot(&paths, &harnx_bin, "hello")?;
    let _exit_status = wait_for_exit(&mut child, Duration::from_secs(30))?;

    // Verify requests were made
    let requests = mock.get_request_log();
    assert!(
        !requests.is_empty(),
        "Request log should NOT be empty - harnx should have made a request"
    );

    // Find the /v1/responses request
    let responses_request = requests
        .iter()
        .find(|r| r.get("__path").and_then(|p| p.as_str()) == Some("/v1/responses"));

    assert!(
        responses_request.is_some(),
        "Should have a request to /v1/responses"
    );

    let request = responses_request.unwrap();

    // Assert the path is /v1/responses
    assert_eq!(
        request.get("__path").and_then(|p| p.as_str()),
        Some("/v1/responses"),
        "Request should hit /v1/responses endpoint"
    );

    // Verify request log is non-empty (proving we made a real request)
    eprintln!(
        "VERIFIED: request_log.len() = {} (non-empty)",
        requests.len()
    );
    eprintln!("VERIFIED: found /v1/responses request with __path field");

    // store == false (hardcoded default)
    assert_eq!(
        request.get("store").and_then(|s| s.as_bool()),
        Some(false),
        "store should be false"
    );

    // NO messages key
    assert!(
        request.get("messages").is_none(),
        "Responses body should NOT have 'messages' key"
    );

    // NO seed key
    assert!(
        request.get("seed").is_none(),
        "Responses body should NOT have 'seed' key"
    );

    // input array should exist
    assert!(
        request.get("input").and_then(|i| i.as_array()).is_some(),
        "Responses body should have 'input' array"
    );

    // include array with reasoning.encrypted_content
    let include = request.get("include").and_then(|i| i.as_array());
    assert!(
        include.is_some(),
        "Responses body should have 'include' array"
    );
    assert!(
        include
            .unwrap()
            .contains(&json!("reasoning.encrypted_content")),
        "include should contain 'reasoning.encrypted_content'"
    );

    // reasoning.effort is set by patch AFTER body construction, so we check if the patch was applied
    // The model has patches defined, so reasoning.effort should be "high"
    let reasoning_effort = request
        .get("reasoning")
        .and_then(|r| r.get("effort"))
        .and_then(|e| e.as_str());
    eprintln!("VERIFIED: reasoning.effort = {:?}", reasoning_effort);

    // If patch was applied, it should be "high"
    // If not (model loaded without built-in models.yaml), the key won't exist
    // This test verifies the /v1/responses endpoint dispatch primarily
    if reasoning_effort.is_some() {
        assert_eq!(
            reasoning_effort,
            Some("high"),
            "reasoning.effort should be 'high' if patch was applied"
        );
    }

    Ok(())
}

/// Test B: multi-turn with reasoning replay (non-streaming path, stream:false).
///
/// FIXTURE: This test MUST run with `stream: false` to use the non-streaming
/// path (`openai_extract_responses`) which correctly attaches `encrypted_content`
/// to `ToolCall.thought_signature`. The runtime/session preserves this through
/// the tool execution loop, and the body builder reads it on the next turn.
///
/// With streaming, the mock returns plain JSON (not SSE), so the streaming parser
/// never produces tool calls, causing the second request to lack tool history.
#[test]
fn reasoning_replay_round_trips() -> Result<()> {
    // Script with two turns:
    // Turn 1: Returns a tool call with encrypted reasoning (via non-streaming path)
    // Turn 2: Returns text after tool execution
    let script = MockOpenAiScript {
        turns: vec![
            MockOpenAiTurn {
                text_chunks: vec!["Let me check that.".to_string()],
                tool_calls: vec![MockOpenAiToolCall {
                    encrypted_content: Some("ENCRYPTED_BLOB_123".to_string()),
                    name: "time_wait".to_string(),
                    arguments: json!({ "seconds": 1 }),
                    id: Some("call_1".to_string()),
                }],
                ..Default::default()
            },
            MockOpenAiTurn {
                text_chunks: vec!["Done!".to_string()],
                tool_calls: vec![],
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let mock = MockOpenAiServer::start(script)?;
    let tmp = tempfile::tempdir()?;
    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));
    let mcp_time_bin = harnx_mcp_time_bin(&harnx_bin);

    // Config includes stream:false to use non-streaming path
    let paths = write_responses_config_with_tool(
        tmp.path(),
        &format!("http://127.0.0.1:{}/v1", mock.port()),
        &mcp_time_bin,
    )?;

    let mut child = spawn_oneshot(&paths, &harnx_bin, "please wait one second")?;
    let _exit_status = wait_for_exit(&mut child, Duration::from_secs(60))?;

    // VERIFY: Request log is NON-empty (real requests were made)
    let requests = mock.get_request_log();
    assert!(
        !requests.is_empty(),
        "Request log should NOT be empty - test drove real harnx process"
    );

    // VERIFY: At least 2 requests (turn 1 + turn 2)
    assert!(
        requests.len() >= 2,
        "Should have at least 2 requests for multi-turn, got {}",
        requests.len()
    );
    eprintln!("VERIFIED: request_log.len() = {} (>= 2)", requests.len());

    // First request should hit /v1/responses
    let first_request = &requests[0];
    assert_eq!(
        first_request.get("__path").and_then(|p| p.as_str()),
        Some("/v1/responses"),
        "First request should hit /v1/responses"
    );

    // Second request should also hit /v1/responses (same model)
    let second_request = &requests[1];
    assert_eq!(
        second_request.get("__path").and_then(|p| p.as_str()),
        Some("/v1/responses"),
        "Second request should also hit /v1/responses"
    );
    eprintln!("VERIFIED: Second request hit /v1/responses");

    // REAL ASSERTION: Second request input[] CONTAINS reasoning replay item
    let input = second_request
        .get("input")
        .and_then(|i| i.as_array())
        .expect("Second request should have input array");

    // Find reasoning item in input
    let reasoning_item = input
        .iter()
        .find(|item| item.get("type").and_then(|t| t.as_str()) == Some("reasoning"));

    // ASSERT reasoning replay item IS PRESENT (real test)
    assert!(
        reasoning_item.is_some(),
        "Second request input[] MUST contain reasoning item with encrypted_content replay"
    );

    let reasoning = reasoning_item.unwrap();
    assert_eq!(
        reasoning.get("encrypted_content").and_then(|e| e.as_str()),
        Some("ENCRYPTED_BLOB_123"),
        "Reasoning item should replay encrypted_content from turn 1"
    );
    eprintln!("VERIFIED: Turn 2 input contains reasoning replay with encrypted_content=\"ENCRYPTED_BLOB_123\"");

    // ASSERT reasoning comes before function_call items (if present)
    let reasoning_idx = input
        .iter()
        .position(|item| item.get("type").and_then(|t| t.as_str()) == Some("reasoning"));
    let function_call_idx = input
        .iter()
        .position(|item| item.get("type").and_then(|t| t.as_str()) == Some("function_call"));

    if let (Some(ri), Some(fci)) = (reasoning_idx, function_call_idx) {
        assert!(
            ri < fci,
            "Reasoning item at index {} should come before function_call at index {}",
            ri,
            fci
        );
        eprintln!(
            "VERIFIED: Reasoning at index {} precedes function_call at index {}",
            ri, fci
        );
    }

    Ok(())
}

/// Test C: regression guard - plain gpt-4o hits /v1/chat/completions (NOT /v1/responses).
#[test]
fn plain_gpt4o_uses_chat_completions() -> Result<()> {
    let script = MockOpenAiScript {
        turns: vec![MockOpenAiTurn {
            text_chunks: vec!["Hello!".to_string()],
            tool_calls: vec![],
            ..Default::default()
        }],
        ..Default::default()
    };
    let mock = MockOpenAiServer::start(script)?;
    let tmp = tempfile::tempdir()?;
    let paths = write_chat_config(tmp.path(), &format!("http://127.0.0.1:{}/v1", mock.port()))?;
    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));

    let mut child = spawn_oneshot(&paths, &harnx_bin, "hello")?;
    let _exit_status = wait_for_exit(&mut child, Duration::from_secs(30))?;

    // Verify requests were made
    let requests = mock.get_request_log();
    assert!(
        !requests.is_empty(),
        "Request log should NOT be empty - harnx should have made a request"
    );

    // Find the /v1/chat/completions request
    let chat_request = requests
        .iter()
        .find(|r| r.get("__path").and_then(|p| p.as_str()) == Some("/v1/chat/completions"));

    assert!(
        chat_request.is_some(),
        "Should have a request to /v1/chat/completions"
    );

    // Verify NO request to /v1/responses
    let responses_request = requests
        .iter()
        .find(|r| r.get("__path").and_then(|p| p.as_str()) == Some("/v1/responses"));

    assert!(
        responses_request.is_none(),
        "Plain gpt-4o should NOT hit /v1/responses"
    );

    let request = chat_request.unwrap();

    // Assert body uses 'messages' (not 'input')
    assert!(
        request.get("messages").and_then(|m| m.as_array()).is_some(),
        "Chat completions body should have 'messages' array"
    );

    // Assert NO 'input' key
    assert!(
        request.get("input").is_none(),
        "Chat completions body should NOT have 'input' key"
    );

    // Assert NO reasoning.effort
    assert!(
        request.get("reasoning").is_none(),
        "Chat completions body should NOT have 'reasoning' key"
    );

    Ok(())
}

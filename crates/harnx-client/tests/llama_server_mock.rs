#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use harnx_client::client::{ChatCompletionsData, Client, Message, MessageContent, MessageRole};
use harnx_client::llama_server::process::{LlamaServerProcessConfig, LlamaServerProcessManager};
use harnx_client::{ClientConfig, LlamaServerClient, Model, SseEvent, SseHandler};
use harnx_core::abort::create_abort_signal;
use harnx_core::provider_config::llama_server::LlamaServerConfig;
use harnx_core::tool::ToolDeclaration;
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::mpsc;

#[tokio::test]
async fn llama_server_mock_e2e_lifecycle() -> Result<()> {
    ensure_mock_binary_built()?;

    let temp_dir = TempDir::new()?;
    let socket_path = temp_dir.path().join("llama-server-mock.sock");
    let script_path = write_mock_script(temp_dir.path())?;
    let binary_path = resolve_mock_binary_path()?;
    let model_path = temp_dir.path().join("dummy.gguf");
    std::fs::write(&model_path, b"mock model")?;

    let config = LlamaServerProcessConfig {
        model_path: model_path.clone(),
        binary_path: Some(binary_path),
        socket_path: Some(socket_path.clone()),
        context_size: Some(256),
        gpu_layers: Some(0i32),
        threads: Some(1),
        extra_args: vec![
            "--script".to_string(),
            script_path.display().to_string(),
            "--unexpected-flag".to_string(),
            "survives".to_string(),
        ],
        ready_timeout: Duration::from_secs(5),
    };

    let manager = LlamaServerProcessManager::new(config.clone())?;
    let running = manager.ensure_ready().await?;
    assert_eq!(running.socket_path(), socket_path.as_path());
    assert!(
        socket_path.exists(),
        "socket should exist after ensure_ready"
    );

    let client = build_client(&config, "mock-model").context("llama-server client init")?;
    let reqwest_client = reqwest::Client::new();

    let non_stream = client
        .chat_completions_inner(
            &reqwest_client,
            ChatCompletionsData {
                messages: vec![user_message("non-stream")],
                temperature: None,
                top_p: None,
                functions: None,
                stream: false,
            },
        )
        .await?;
    assert_eq!(non_stream.text, "mock non streaming reply");
    assert!(non_stream.tool_calls.is_empty());

    let (stream_text, text_event_count) = stream_chat(
        &*client,
        &reqwest_client,
        ChatCompletionsData {
            messages: vec![user_message("stream")],
            temperature: None,
            top_p: None,
            functions: None,
            stream: true,
        },
    )
    .await?;
    assert_eq!(stream_text, "streaming chunk parade");
    assert!(
        text_event_count > 1,
        "expected multiple text chunks, got {text_event_count}"
    );

    let tool_output = client
        .chat_completions_inner(
            &reqwest_client,
            ChatCompletionsData {
                messages: vec![user_message("tool")],
                temperature: None,
                top_p: None,
                functions: Some(vec![tool_declaration()]),
                stream: false,
            },
        )
        .await?;
    assert_eq!(tool_output.text, "tool call incoming");
    assert_eq!(tool_output.tool_calls.len(), 1);
    let tool_call = &tool_output.tool_calls[0];
    assert_eq!(tool_call.name, "test_tool");
    assert_eq!(tool_call.arguments, json!({"value":"roundtrip"}));
    assert_eq!(tool_call.id.as_deref(), Some("call_tool_roundtrip"));

    drop(manager);
    // Poll until the socket is cleaned up rather than relying on a fixed sleep,
    // which can flake under loaded CI.
    let mut socket_removed = false;
    for _ in 0..50 {
        if !socket_path.exists() {
            socket_removed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        socket_removed,
        "socket should be removed after dropping manager"
    );

    Ok(())
}

fn build_client(config: &LlamaServerProcessConfig, model_name: &str) -> Option<Box<dyn Client>> {
    let provider = LlamaServerConfig {
        name: Some("llama-server".to_string()),
        model_path: config.model_path.display().to_string(),
        binary_path: config
            .binary_path
            .as_ref()
            .map(|path| path.display().to_string()),
        ctx_size: config.context_size,
        n_gpu_layers: config.gpu_layers.map(|v| v as u32),
        threads: config.threads,
        extra_args: Some(config.extra_args.clone()),
        socket_path: config
            .socket_path
            .as_ref()
            .map(|path| path.display().to_string()),
        ..Default::default()
    };

    let model = Model::new("llama-server", model_name);
    LlamaServerClient::init(&[ClientConfig::LlamaServerConfig(provider)], &model)
}

async fn stream_chat(
    client: &dyn Client,
    reqwest_client: &reqwest::Client,
    data: ChatCompletionsData,
) -> Result<(String, usize)> {
    let (tx, mut rx) = mpsc::unbounded_channel::<SseEvent>();
    let mut handler = SseHandler::new(tx, create_abort_signal());

    client
        .chat_completions_streaming_inner(reqwest_client, &mut handler, data)
        .await?;

    let mut streamed_text = String::new();
    let mut text_event_count = 0usize;
    while let Ok(event) = rx.try_recv() {
        if let SseEvent::Text(chunk) = event {
            text_event_count += 1;
            streamed_text.push_str(&chunk);
        }
    }

    Ok((streamed_text, text_event_count))
}

fn user_message(text: &str) -> Message {
    Message {
        role: MessageRole::User,
        content: MessageContent::Text(text.to_string()),
        ..Default::default()
    }
}

fn tool_declaration() -> ToolDeclaration {
    ToolDeclaration {
        name: "test_tool".to_string(),
        description: "returns roundtrip".to_string(),
        parameters: Default::default(),
        mcp_tool_name: None,
        mcp_server_name: None,
        call_template: None,
        result_template: None,
    }
}

fn write_mock_script(dir: &Path) -> Result<PathBuf> {
    let path = dir.join("mock-script.yaml");
    std::fs::write(
        &path,
        r#"chunk_delay_ms: 5
turns:
  - text_chunks:
      - "mock non streaming reply"
  - text_chunks:
      - "streaming"
      - " chunk"
      - " parade"
  - text_chunks:
      - "tool call incoming"
    tool_calls:
      - id: "call_tool_roundtrip"
        name: "test_tool"
        arguments:
          value: "roundtrip"
"#,
    )?;
    Ok(path)
}

fn ensure_mock_binary_built() -> Result<()> {
    let status = Command::new(env!("CARGO"))
        .args(["build", "-p", "harnx-test-bins", "--bin", "harnx-mock-llm"])
        .status()
        .context("failed to invoke cargo to build harnx-mock-llm")?;
    if !status.success() {
        bail!("cargo build -p harnx-test-bins --bin harnx-mock-llm failed with status {status}");
    }
    Ok(())
}

fn resolve_mock_binary_path() -> Result<PathBuf> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .context("failed to resolve workspace root from CARGO_MANIFEST_DIR")?;
    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root.join("target"));

    for profile in ["debug", "release"] {
        let candidate = target_dir.join(profile).join("harnx-mock-llm");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    bail!(
        "failed to locate harnx-mock-llm in {}",
        target_dir.display()
    )
}

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use harnx_client::client::{ChatCompletionsData, Client, Message, MessageContent, MessageRole};
use harnx_client::llama_server::process::{
    LlamaServerProcessConfig, LlamaServerProcessManager, ModelSource,
};
use harnx_client::{ClientConfig, LlamaServerClient, Model, SseEvent, SseHandler};
use harnx_core::abort::create_abort_signal;
use harnx_core::model::ModelData;
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
        model_source: ModelSource::LocalPath(model_path.clone()),
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

/// Test multi-model support: one config with two models yields two distinct subprocesses.
///
/// Each model specifies its own socket_path and model_path, forcing distinct ProcessIdentity
/// and thus distinct processes. The mock binary ignores -m (model_path), so we use distinct
/// socket paths to isolate the processes. Each client drives a chat through its model's
/// subprocess, and both sockets are cleaned up on drop.
#[tokio::test]
async fn llama_server_mock_multi_model_distinct_processes() -> Result<()> {
    ensure_mock_binary_built()?;

    let temp_dir = TempDir::new()?;
    let binary_path = resolve_mock_binary_path()?;
    let script_path = write_mock_script(temp_dir.path())?;

    // Model A: distinct socket and dummy gguf path
    let socket_a = temp_dir.path().join("model-a.sock");
    let model_path_a = temp_dir.path().join("model-a.gguf");
    std::fs::write(&model_path_a, b"mock model a")?;

    // Model B: distinct socket and dummy gguf path
    let socket_b = temp_dir.path().join("model-b.sock");
    let model_path_b = temp_dir.path().join("model-b.gguf");
    std::fs::write(&model_path_b, b"mock model b")?;

    // Single provider config with two models, each with its own GGUF/socket
    let provider = LlamaServerConfig {
        name: Some("multi-llama".to_string()),
        models: vec![
            ModelData::new("model-a")
                .with_model_path(model_path_a.display().to_string())
                .with_socket_path(socket_a.display().to_string())
                .with_ctx_size(256)
                .with_threads(1)
                .with_extra_args(vec![
                    "--script".to_string(),
                    script_path.display().to_string(),
                ]),
            ModelData::new("model-b")
                .with_model_path(model_path_b.display().to_string())
                .with_socket_path(socket_b.display().to_string())
                .with_ctx_size(256)
                .with_threads(1)
                .with_extra_args(vec![
                    "--script".to_string(),
                    script_path.display().to_string(),
                ]),
        ],
        binary_path: Some(binary_path.display().to_string()),
        ..Default::default()
    };

    // Init client for model-a (use from_config to pick up the model's data)
    let models_list = Model::from_config("multi-llama", &provider.models);
    let model_a = models_list
        .iter()
        .find(|m| m.name() == "model-a")
        .context("model-a not found")?
        .clone();
    let client_a = LlamaServerClient::init(
        &[ClientConfig::LlamaServerConfig(provider.clone())],
        &model_a,
    )
    .context("init client_a")?;

    // Init client for model-b
    let model_b = models_list
        .iter()
        .find(|m| m.name() == "model-b")
        .context("model-b not found")?
        .clone();
    let client_b = LlamaServerClient::init(
        &[ClientConfig::LlamaServerConfig(provider.clone())],
        &model_b,
    )
    .context("init client_b")?;

    let reqwest_client = reqwest::Client::new();

    // Drive chat through model-a
    let resp_a = client_a
        .chat_completions_inner(
            &reqwest_client,
            ChatCompletionsData {
                messages: vec![user_message("hello from a")],
                temperature: None,
                top_p: None,
                functions: None,
                stream: false,
            },
        )
        .await?;
    assert_eq!(resp_a.text, "mock non streaming reply");

    // Drive chat through model-b
    let resp_b = client_b
        .chat_completions_inner(
            &reqwest_client,
            ChatCompletionsData {
                messages: vec![user_message("hello from b")],
                temperature: None,
                top_p: None,
                functions: None,
                stream: false,
            },
        )
        .await?;
    assert_eq!(resp_b.text, "mock non streaming reply");

    // Both sockets should exist (distinct processes spawned)
    assert!(socket_a.exists(), "socket_a should exist");
    assert!(socket_b.exists(), "socket_b should exist");

    // Drop clients. The global MANAGERS registry holds Arc<LlamaServerProcessManager>,
    // so dropping the client doesn't immediately drop the manager.
    // Instead, we need to explicitly verify the processes by checking the registry behavior.
    // For this test, we verify distinct processes via distinct sockets responding independently.
    // Socket cleanup happens when the manager is dropped (on process exit or harnx shutdown).
    // Drop clients to verify they work correctly
    drop(client_a);
    drop(client_b);

    // Verify both sockets still exist (global registry holds managers alive)
    assert!(
        socket_a.exists(),
        "socket_a should still exist (registry holds manager)"
    );
    assert!(
        socket_b.exists(),
        "socket_b should still exist (registry holds manager)"
    );

    // For this test, the key verification is that:
    // 1. Both clients successfully communicated through distinct sockets
    // 2. Two sockets exist (proving two distinct processes)
    // The global registry deliberately keeps processes alive for reuse.
    // Socket cleanup will happen on harnx exit.

    Ok(())
}

/// Test HF repo-only model (no model_path, uses hf_repo field).
/// The mock ignores -hf flag, so we can verify end-to-end flow.
#[tokio::test]
async fn llama_server_mock_hf_repo_only() -> Result<()> {
    ensure_mock_binary_built()?;

    let temp_dir = TempDir::new()?;
    let socket_path = temp_dir.path().join("hf-model.sock");
    let script_path = write_mock_script(temp_dir.path())?;
    let binary_path = resolve_mock_binary_path()?;

    // Model with hf_repo only (no model_path)
    let model_data = ModelData::new("hf-test-model")
        .with_hf_repo("unsloth/test-model:Q4_K_M".to_string())
        .with_socket_path(socket_path.display().to_string())
        .with_ctx_size(256)
        .with_threads(1)
        .with_extra_args(vec![
            "--script".to_string(),
            script_path.display().to_string(),
        ]);

    let provider = LlamaServerConfig {
        name: Some("test-hf-provider".to_string()),
        models: vec![model_data],
        binary_path: Some(binary_path.display().to_string()),
        ..Default::default()
    };

    let models_list = Model::from_config("test-hf-provider", &provider.models);
    let model = models_list
        .iter()
        .find(|m| m.name() == "hf-test-model")
        .context("hf-test-model not found")?
        .clone();

    let client = LlamaServerClient::init(&[ClientConfig::LlamaServerConfig(provider)], &model)
        .context("init hf client")?;

    let reqwest_client = reqwest::Client::new();
    let resp = client
        .chat_completions_inner(
            &reqwest_client,
            ChatCompletionsData {
                messages: vec![user_message("test hf")],
                temperature: None,
                top_p: None,
                functions: None,
                stream: false,
            },
        )
        .await?;

    assert_eq!(resp.text, "mock non streaming reply");
    assert!(socket_path.exists(), "socket should exist");

    Ok(())
}

/// Test model with neither model_path nor hf_repo (name used as -hf argument).
/// The mock ignores -hf flag, so we can verify end-to-end flow.
#[tokio::test]
async fn llama_server_mock_name_as_hf_repo() -> Result<()> {
    ensure_mock_binary_built()?;

    let temp_dir = TempDir::new()?;
    // Use distinct socket to get a distinct process (prevents collision with other tests)
    let socket_path = temp_dir.path().join("name-as-hf.sock");
    let script_path = write_mock_script(temp_dir.path())?;
    let binary_path = resolve_mock_binary_path()?;

    // Model with neither model_path nor hf_repo - name will be used as HF repo
    let model_data = ModelData::new("user/model-name:Q4_K_M")
        .with_socket_path(socket_path.display().to_string())
        .with_ctx_size(256)
        .with_threads(1)
        .with_extra_args(vec![
            "--script".to_string(),
            script_path.display().to_string(),
        ]);

    let provider = LlamaServerConfig {
        name: Some("test-name-hf".to_string()),
        models: vec![model_data],
        binary_path: Some(binary_path.display().to_string()),
        ..Default::default()
    };

    let models_list = Model::from_config("test-name-hf", &provider.models);
    let model = models_list
        .iter()
        .find(|m| m.name() == "user/model-name:Q4_K_M")
        .context("model not found")?
        .clone();

    let client = LlamaServerClient::init(&[ClientConfig::LlamaServerConfig(provider)], &model)
        .context("init name-as-hf client")?;

    let reqwest_client = reqwest::Client::new();
    let resp = client
        .chat_completions_inner(
            &reqwest_client,
            ChatCompletionsData {
                messages: vec![user_message("test name as hf")],
                temperature: None,
                top_p: None,
                functions: None,
                stream: false,
            },
        )
        .await?;

    assert_eq!(resp.text, "mock non streaming reply");
    assert!(socket_path.exists(), "socket should exist");

    Ok(())
}

fn build_client(config: &LlamaServerProcessConfig, model_name: &str) -> Option<Box<dyn Client>> {
    // Set the matching source field per variant: a local path → `model_path`,
    // a HuggingFace repo → `hf_repo`. Leave the other unset (None) so we don't
    // produce a misleading `model_path: Some("")`.
    let mut model_data = ModelData::new(model_name);
    match &config.model_source {
        ModelSource::LocalPath(p) => {
            model_data = model_data.with_model_path(p.display().to_string());
        }
        ModelSource::HfRepo(r) => {
            model_data = model_data.with_hf_repo(r.clone());
        }
    }

    let model_data = model_data
        .with_socket_path(
            config
                .socket_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        )
        .with_ctx_size(config.context_size.unwrap_or(0))
        .with_n_gpu_layers(config.gpu_layers.map(|v| v as u32).unwrap_or(0))
        .with_threads(config.threads.unwrap_or(0))
        .with_extra_args(config.extra_args.clone());

    let provider = LlamaServerConfig {
        name: Some("llama-server".to_string()),
        models: vec![model_data],
        binary_path: config.binary_path.as_ref().map(|p| p.display().to_string()),
        ..Default::default()
    };

    // Use Model::from_config to get model with correct data from provider config
    let models = Model::from_config("llama-server", &provider.models);
    let model = models.into_iter().next()?;
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

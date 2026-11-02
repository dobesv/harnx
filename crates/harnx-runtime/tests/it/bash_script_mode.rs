//! Script-mode `bash` e2e through the runtime's real wiring: config
//! resolution → synthesized declaration → `BashProvider` at provider index 0
//! → slot handoff → BashKit run → inner NATS tool dispatch → engine result.

// Keep imports and fixtures under the same Unix gate as the e2e tests.
#![cfg(unix)]

use std::{
    ffi::OsString,
    path::PathBuf,
    process::Command,
    sync::Arc,
    time::{Duration, Instant},
};

use crate::common;
use anyhow::{Context as _, Result};
use harnx_core::abort::create_abort_signal;
use harnx_core::instance::ServerScope;
use harnx_core::tool::ToolCall;
use harnx_engine::tool::eval_tool_calls;
use harnx_runtime::bash_provider::BashEvalContextSlot;
use harnx_runtime::config::{
    agent::Agent, AgentConfig, Config, ConfigLock, ToolServerConfig, HARNX_NATS_TOKEN_ENV,
    HARNX_NATS_URL_ENV,
};
use harnx_runtime::nats_worker::{ToolServerStartConfig, ToolServerSupervisor};
use harnx_runtime::tool::BuildToolEvalContextParams;
use serde_json::json;

const TOKEN: &str = "bash-script-mode-test-token";

struct EnvGuard(Vec<(&'static str, Option<OsString>)>);

impl EnvGuard {
    fn install(values: &[(&'static str, &str)]) -> Self {
        let old = values
            .iter()
            .map(|(name, _)| (*name, std::env::var_os(name)))
            .collect();
        for (name, value) in values {
            // SAFETY: test-scoped env manipulation, same pattern as
            // tool_supervisor tests.
            unsafe { std::env::set_var(name, value) };
        }
        Self(old)
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, value) in self.0.iter().rev() {
            // SAFETY: test-scoped env restoration.
            unsafe {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }
}

fn time_server_config(command: &str) -> ToolServerConfig {
    ToolServerConfig {
        name: "time".to_string(),
        command: command.into(),
        args: Vec::new(),
        env: Default::default(),
        enabled: true,
        description: None,
        package: None,
        hooks: None,
    }
}

fn time_server_binary() -> Result<PathBuf> {
    if let Some(path) = option_env!("CARGO_BIN_EXE_harnx-time-tools") {
        return Ok(PathBuf::from(path));
    }
    let mut path = std::env::current_exe().context("resolve test executable")?;
    path.pop();
    if path.file_name().is_some_and(|name| name == "deps") {
        path.pop();
    }
    path.push(if cfg!(windows) {
        "harnx-time-tools.exe"
    } else {
        "harnx-time-tools"
    });
    if path.is_file() {
        return Ok(path);
    }
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .context("resolve workspace root")?
        .to_path_buf();
    let status = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .arg("build")
        .arg("-p")
        .arg("harnx-time-tools")
        .current_dir(workspace)
        .status()
        .context("build harnx-time-tools for script-mode e2e")?;
    anyhow::ensure!(status.success(), "building harnx-time-tools failed");
    anyhow::ensure!(
        path.is_file(),
        "harnx-time-tools not found at {}",
        path.display()
    );
    Ok(path)
}

/// Bash-only agent: `bash_tools: time_*` and no `use_tools`.
fn script_mode_config() -> Arc<ConfigLock> {
    let mut agent = AgentConfig::from_prompt("test");
    agent.set_bash_tools(Some(vec!["time_*".to_string()]));
    let config = Config {
        agent: Some(Agent::new(agent)),
        ..Config::default()
    };
    Arc::new(ConfigLock::new(config))
}

async fn wait_for_time_registration(
    client: &async_nats::Client,
    instance_id: &ServerScope,
) -> Result<()> {
    let store = async_nats::jetstream::new(client.clone())
        .get_key_value(harnx_toolset_server::TOOL_REGISTRY_BUCKET)
        .await?;
    let key = harnx_toolset_server::registration_key(instance_id, "__time__time");
    let deadline = Instant::now() + Duration::from_secs(5);
    while store.get(&key).await?.is_none() {
        anyhow::ensure!(
            Instant::now() < deadline,
            "time tool server never registered"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Ok(())
}

fn assert_time_tools_active(config: &ConfigLock) {
    assert!(
        config.read().bash_tools_active(),
        "bash_tools: time_* must activate script mode once declarations are cached"
    );
    let bash_tools = config.read().bash_inner_tool_declarations();
    assert!(
        bash_tools
            .iter()
            .any(|declaration| declaration.name == "time_get_current_time"),
        "bash_tools must resolve the NATS time tools, got {bash_tools:?}"
    );
}

/// Full-stack e2e: a real NATS broker, a real tool server process, a real
/// agent config, the real runtime context construction, and a `bash` call
/// that runs `time_get_current_time … | wc -l` through BashKit and the
/// inner-call admission pipeline.
#[tokio::test(flavor = "multi_thread")]
async fn bash_runs_nats_tool_pipeline_e2e() -> Result<()> {
    let Some(server) = common::spawn_nats_server_with_options(common::SpawnNatsServerOptions {
        auth_token: Some(TOKEN.to_string()),
    })
    .await?
    else {
        return Ok(());
    };
    let binary = time_server_binary()?.to_string_lossy().into_owned();
    let _env = EnvGuard::install(&[
        (HARNX_NATS_URL_ENV, server.url()),
        (HARNX_NATS_TOKEN_ENV, TOKEN),
    ]);
    let client = async_nats::ConnectOptions::new()
        .token(TOKEN.to_string())
        .connect(server.url())
        .await?;
    let instance_id = ServerScope::new();
    let start =
        ToolServerStartConfig::new(client.clone(), instance_id.clone(), server.url(), TOKEN);
    let servers = [time_server_config(&binary)];
    let supervisor =
        ToolServerSupervisor::start_local_with_timeout(start, &servers, Duration::from_secs(5))
            .await?;
    wait_for_time_registration(&client, &instance_id).await?;

    let config = script_mode_config();
    // The declaration cache is what bash_tools resolution reads; the agent
    // loop refreshes it before tool rounds, and so must this test.
    harnx_runtime::tool_context::refresh_nats_tool_declarations(&config, &instance_id).await;
    assert_time_tools_active(&config);

    let slot = Arc::new(BashEvalContextSlot::default());
    let mut params = BuildToolEvalContextParams::new(&config, &instance_id);
    params.bash_context_slot = Some(slot.clone());
    let ctx = Arc::new(harnx_runtime::tool::build_tool_eval_context(params).await);
    slot.set(&ctx);
    assert!(
        ctx.providers[0].has_tool("bash"),
        "BashProvider must lead the provider list"
    );

    let abort = create_abort_signal();
    let results = eval_tool_calls(
        &ctx,
        vec![ToolCall::new(
            "bash".to_string(),
            json!({"script": "time_get_current_time --timezone UTC | wc -l"}),
            Some("bash-1".to_string()),
            None,
        )],
        &abort,
    )
    .await?;
    drop(supervisor);
    assert_eq!(results.len(), 1);
    let output = &results[0].output;
    assert_eq!(output["status"], "ok", "{}", output);
    assert_eq!(output["exit_code"], 0);
    assert_eq!(output["trace"][0]["name"], "time_get_current_time");
    assert_eq!(output["trace"][0]["status"], "ok");
    // One JSON result line through `wc -l`.
    let stdout = output["stdout"].as_str().expect("stdout text");
    assert_eq!(stdout.trim(), "1", "{}", output);
    let text = output["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("status: ok"), "{text}");
    assert!(text.contains("<!-- start stdout -->"), "{text}");
    Ok(())
}

/// Recovery: after worker loss the engine replays the call across providers
/// in order; an active BashProvider must answer `bash` with the interrupted
/// report so the script is never rerun (J-RHAD-r2-1). Active means the
/// agent's `bash_tools` actually resolve against registered declarations
/// (PR3 review F9), so this test stands up the real NATS broker and time
/// tool server like the pipeline e2e above. It needs no dispatch: the
/// provider answers from its own policy before any remote replay.
#[tokio::test(flavor = "multi_thread")]
async fn interrupted_bash_replays_as_reported_not_rerun() -> Result<()> {
    let Some(server) = common::spawn_nats_server_with_options(common::SpawnNatsServerOptions {
        auth_token: Some(TOKEN.to_string()),
    })
    .await?
    else {
        return Ok(());
    };
    let binary = time_server_binary()?.to_string_lossy().into_owned();
    let _env = EnvGuard::install(&[
        (HARNX_NATS_URL_ENV, server.url()),
        (HARNX_NATS_TOKEN_ENV, TOKEN),
    ]);
    let client = async_nats::ConnectOptions::new()
        .token(TOKEN.to_string())
        .connect(server.url())
        .await?;
    let instance_id = ServerScope::new();
    let start =
        ToolServerStartConfig::new(client.clone(), instance_id.clone(), server.url(), TOKEN);
    let servers = [time_server_config(&binary)];
    let supervisor =
        ToolServerSupervisor::start_local_with_timeout(start, &servers, Duration::from_secs(5))
            .await?;
    wait_for_time_registration(&client, &instance_id).await?;

    let config = script_mode_config();
    harnx_runtime::tool_context::refresh_nats_tool_declarations(&config, &instance_id).await;
    assert!(
        config.read().bash_tools_active(),
        "bash_tools: time_* must activate script mode once declarations are cached"
    );
    let slot = Arc::new(BashEvalContextSlot::default());
    let mut params = BuildToolEvalContextParams::new(&config, &instance_id);
    params.bash_context_slot = Some(slot.clone());
    let ctx = Arc::new(harnx_runtime::tool::build_tool_eval_context(params).await);
    slot.set(&ctx);
    let call = ToolCall::new(
        "bash".to_string(),
        json!({"script": "time_get_current_time --timezone UTC"}),
        Some("bash-1".to_string()),
        None,
    );
    let abort = create_abort_signal();
    let replay = harnx_engine::tool::replay_tool_call(
        &ctx,
        harnx_core::tool::ToolReplay {
            session_id: "session-1",
            tool_round: 1,
            call: &call,
            worker_id: Some("worker-1"),
            fence_token: None,
            authorization: None,
        },
        &abort,
    )
    .await
    .expect("replay evaluates");
    let result = replay.expect("active script mode must answer bash replay as interrupted");
    assert_eq!(result.output["status"], "interrupted", "{}", result.output);
    let message = result.output["error"].as_str().unwrap();
    assert!(
        message.contains("interrupted") && message.contains("not resumed"),
        "{message}"
    );
    drop(supervisor);
    Ok(())
}

/// PR3 review F9: when no declarations resolve (script mode inactive), even
/// a bare `bash` call must fall through replay untouched so a
/// tool-server-native `bash` keeps normal NATS journal recovery. The
/// provider-internal halves of both cases are covered in
/// `bash_provider::tests`; this pins the engine-level fall-through.
#[tokio::test(flavor = "multi_thread")]
async fn inactive_script_mode_leaves_bash_replay_to_other_providers() -> Result<()> {
    let config = script_mode_config();
    // No tool servers: `bash_tools: time_*` resolves to nothing, so the
    // synthesized declaration — and the provider's active flag — stay off.
    assert!(
        !config.read().bash_tools_active(),
        "no registered declarations means script mode is inactive"
    );
    let slot = Arc::new(BashEvalContextSlot::default());
    let instance_id = ServerScope::new();
    let mut params = BuildToolEvalContextParams::new(&config, &instance_id);
    params.bash_context_slot = Some(slot.clone());
    let ctx = Arc::new(harnx_runtime::tool::build_tool_eval_context(params).await);
    slot.set(&ctx);
    let call = ToolCall::new(
        "bash".to_string(),
        json!({"script": "time_get_current_time --timezone UTC"}),
        Some("bash-1".to_string()),
        None,
    );
    let abort = create_abort_signal();
    let replay = harnx_engine::tool::replay_tool_call(
        &ctx,
        harnx_core::tool::ToolReplay {
            session_id: "session-1",
            tool_round: 1,
            call: &call,
            worker_id: Some("worker-1"),
            fence_token: None,
            authorization: None,
        },
        &abort,
    )
    .await
    .expect("replay evaluates");
    assert!(
        replay.is_none(),
        "inactive script mode must not claim bash replay"
    );
    Ok(())
}

/// PR3 round-2 review R4: the fall-through above must actually reach NATS
/// journal recovery. Seeds the journal the way a completed tool-server-native
/// `bash` call leaves it — one row under (session, round, tool-call id) with
/// its final reply — and asserts replay hands back the saved reply instead
/// of a synthetic interruption.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn inactive_script_mode_bash_replay_recovers_journaled_reply() -> Result<()> {
    let Some(server) = common::spawn_nats_server_with_options(common::SpawnNatsServerOptions {
        auth_token: Some(TOKEN.to_string()),
    })
    .await?
    else {
        return Ok(());
    };
    let _env = EnvGuard::install(&[
        (HARNX_NATS_URL_ENV, server.url()),
        (HARNX_NATS_TOKEN_ENV, TOKEN),
    ]);
    let client = async_nats::ConnectOptions::new()
        .token(TOKEN.to_string())
        .connect(server.url())
        .await?;

    // Seed the invocation journal exactly like a finished external call.
    let request = harnx_toolset::ToolRequest {
        run_context: None,
        replay: None,
        call_id: "wire-1".to_string(),
        operation_id: "wire-1".to_string(),
        tool: "bash".to_string(),
        args: json!({"command": "true"}),
        parent_session_id: Some("session-1".to_string()),
        parent_agent: None,
        parent_local_session_id: None,
        tool_call_id: Some("bash-1".to_string()),
        capabilities: Default::default(),
        tool_round: Some(1),
    };
    let journal = harnx_toolset_server::invocation_journal::InvocationJournal::ensure(
        &async_nats::jetstream::new(client.clone()),
        1,
    )
    .await?;
    journal
        .record(&request, ("bash", "external-scope", "external-server"))
        .await?;
    journal
        .complete(
            &request,
            harnx_toolset::ToolReply {
                call_id: "wire-1".to_string(),
                result: Ok(json!({"native": true})),
                final_progress: None,
            },
        )
        .await?;

    // No tool servers: script mode stays inactive, so the bare `bash` name
    // belongs to the journaled external tool.
    let config = script_mode_config();
    assert!(
        !config.read().bash_tools_active(),
        "no registered declarations means script mode is inactive"
    );
    let slot = Arc::new(BashEvalContextSlot::default());
    let instance_id = ServerScope::new();
    let mut params = BuildToolEvalContextParams::new(&config, &instance_id);
    params.bash_context_slot = Some(slot.clone());
    let ctx = Arc::new(harnx_runtime::tool::build_tool_eval_context(params).await);
    slot.set(&ctx);
    let call = ToolCall::new(
        "bash".to_string(),
        json!({"script": "true"}),
        Some("bash-1".to_string()),
        None,
    );
    let abort = create_abort_signal();
    let replay = harnx_engine::tool::replay_tool_call(
        &ctx,
        harnx_core::tool::ToolReplay {
            session_id: "session-1",
            tool_round: 1,
            call: &call,
            worker_id: Some("worker-2"),
            fence_token: None,
            authorization: None,
        },
        &abort,
    )
    .await
    .expect("replay evaluates");
    let result =
        replay.expect("inactive script mode must let the journal answer for the external bash");
    assert_eq!(
        result.output,
        json!({"native": true}),
        "the external tool's saved reply must be recovered, not a synthetic interruption: {}",
        result.output
    );
    Ok(())
}

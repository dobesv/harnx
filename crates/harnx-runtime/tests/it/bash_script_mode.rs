//! Script-mode `bash` e2e through the runtime's real wiring: config
//! resolution → synthesized declaration → `BashProvider` at provider index 0
//! → slot handoff → BashKit run → inner NATS tool dispatch → engine result.

// Keep imports and fixtures under the same Unix gate as the e2e tests.
#![cfg(unix)]

mod session_tools;

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

mod grep;
mod plans;

const TOKEN: &str = "bash-script-mode-test-token";

#[path = "bash_script_mode/attachments.rs"]
mod attachments;

pub(crate) struct EnvGuard(Vec<(&'static str, Option<OsString>)>);

impl EnvGuard {
    pub(crate) fn install(values: &[(&'static str, &str)]) -> Self {
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

pub(crate) fn time_server_config(command: &str) -> ToolServerConfig {
    ToolServerConfig {
        middleware: Default::default(),
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

pub(crate) fn time_server_binary() -> Result<PathBuf> {
    if let Some(path) = option_env!("CARGO_BIN_EXE_harnx-time-tools") {
        return Ok(PathBuf::from(path));
    }
    workspace_binary("harnx-time-tools")
}

/// The built binary of a workspace package, built on demand when the test
/// binary's own target directory does not have it yet.
///
/// The on-demand build only runs when the binary is *missing*, so after
/// editing a library crate, `cargo nextest` rebuilds the test binaries but
/// leaves a previously built sibling (e.g. `harnx-time-tools`,
/// `harnx-mcp-bridge`) at its old code — a bridged e2e can then pass
/// against a stale binary and hide the edit's effect. Rebuild the spawned
/// packages explicitly (`cargo build -p <pkg>`) before judging local e2e
/// results; CI is immune because its archive carries fresh bins.
fn workspace_binary(package: &str) -> Result<PathBuf> {
    let mut path = std::env::current_exe().context("resolve test executable")?;
    path = harnx_core::bin_dir_of_exe(&path);
    path.push(if cfg!(windows) {
        format!("{package}.exe")
    } else {
        package.to_string()
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
        .arg(package)
        .current_dir(workspace)
        .status()
        .with_context(|| format!("build {package} for script-mode e2e"))?;
    anyhow::ensure!(status.success(), "building {package} failed");
    anyhow::ensure!(path.is_file(), "{package} not found at {}", path.display());
    Ok(path)
}

/// Start `servers` under `instance_id` against a token-authenticated broker
/// and wait for the time tool's registration. Both script-mode e2es and the
/// script-tools cold-start e2e need this exact sequence; one copy keeps the
/// fixtures identical.
pub(crate) async fn start_time_server(
    server_url: &str,
    token: &str,
    instance_id: &ServerScope,
    servers: &[ToolServerConfig],
) -> Result<ToolServerSupervisor> {
    start_tool_servers(server_url, token, instance_id, servers, &["__time__time"]).await
}

/// As [`start_time_server`], waiting for the registration `identity` names.
async fn start_tool_servers(
    server_url: &str,
    token: &str,
    instance_id: &ServerScope,
    servers: &[ToolServerConfig],
    identities: &[&str],
) -> Result<ToolServerSupervisor> {
    let client = async_nats::ConnectOptions::new()
        .token(token.to_string())
        .connect(server_url)
        .await?;
    let start = ToolServerStartConfig::new(client.clone(), instance_id.clone(), server_url, token);
    let supervisor =
        ToolServerSupervisor::start_local_with_timeout(start, servers, Duration::from_secs(60))
            .await?;
    for identity in identities {
        wait_for_registration(&client, instance_id, identity).await?;
    }
    Ok(supervisor)
}

/// Bash-only agent: `bash_tools` set to `patterns` and no `use_tools`.
pub(crate) fn script_mode_config(patterns: &[&str]) -> Arc<ConfigLock> {
    let mut agent = AgentConfig::from_prompt("test");
    agent.set_bash_tools(Some(patterns.iter().map(|p| p.to_string()).collect()));
    let config = Config {
        agent: Some(Agent::new(agent)),
        ..Config::default()
    };
    Arc::new(ConfigLock::new(config))
}

async fn wait_for_registration(
    client: &async_nats::Client,
    instance_id: &ServerScope,
    identity: &str,
) -> Result<()> {
    let store = async_nats::jetstream::new(client.clone())
        .get_key_value(harnx_toolset_server::TOOL_REGISTRY_BUCKET)
        .await?;
    let key = harnx_toolset_server::registration_key(instance_id, identity);
    let deadline = Instant::now() + Duration::from_secs(60);
    while store.get(&key).await?.is_none() {
        anyhow::ensure!(
            Instant::now() < deadline,
            "tool server '{identity}' never registered"
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

/// Start one tool server and an agent whose `bash_tools` select `patterns`,
/// with the server's declarations cached the way the agent loop caches them
/// before a tool round.
async fn start_script_server(
    server_url: &str,
    server: ToolServerConfig,
    identity: &str,
    patterns: &[&str],
) -> Result<(ServerScope, ToolServerSupervisor, Arc<ConfigLock>)> {
    let instance_id = ServerScope::new();
    let supervisor =
        start_tool_servers(server_url, TOKEN, &instance_id, &[server], &[identity]).await?;
    let config = script_mode_config(patterns);
    harnx_runtime::tool_context::refresh_nats_tool_declarations(&config, &instance_id).await;
    Ok((instance_id, supervisor, config))
}

/// Run one `bash` call for `script` through the runtime's real wiring and
/// return its output.
pub(crate) async fn run_bash_script(
    config: &Arc<ConfigLock>,
    instance_id: &ServerScope,
    script: &str,
) -> Result<serde_json::Value> {
    let slot = Arc::new(BashEvalContextSlot::default());
    let mut params = BuildToolEvalContextParams::new(config, instance_id);
    params.bash_context_slot = Some(slot.clone());
    let ctx = Arc::new(harnx_runtime::tool::build_tool_eval_context(params).await);
    slot.set(&ctx);
    assert!(
        ctx.providers[0].has_tool("bash"),
        "BashProvider must lead the provider list"
    );

    let abort = create_abort_signal();
    let mut results = eval_tool_calls(
        &ctx,
        vec![ToolCall::new(
            "bash".to_string(),
            json!({"script": script}),
            Some("bash-1".to_string()),
            None,
        )],
        &abort,
    )
    .await?;
    anyhow::ensure!(results.len() == 1, "one bash call, one result");
    Ok(results.remove(0).output)
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
    let instance_id = ServerScope::new();
    let supervisor = start_time_server(
        server.url(),
        TOKEN,
        &instance_id,
        &[time_server_config(&binary)],
    )
    .await?;

    let config = script_mode_config(&["time_*"]);
    // The declaration cache is what bash_tools resolution reads; the agent
    // loop refreshes it before tool rounds, and so must this test.
    harnx_runtime::tool_context::refresh_nats_tool_declarations(&config, &instance_id).await;
    assert_time_tools_active(&config);

    let output = run_bash_script(
        &config,
        &instance_id,
        "time_get_current_time --timezone UTC | wc -l",
    )
    .await?;
    drop(supervisor);
    let output = &output;
    assert_eq!(output["status"], "ok", "{}", output);
    assert_eq!(output["exit_code"], 0);
    assert!(output.get("stdout").is_none(), "{}", output);
    let text = output["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("status: ok"), "{text}");
    assert!(text.contains("- time_get_current_time ok"), "{text}");
    // One JSON result line through `wc -l`.
    let stdout_block = text
        .split("<!-- start stdout -->")
        .nth(1)
        .and_then(|rest| rest.split("<!-- end stdout -->").next())
        .expect("stdout block");
    assert!(stdout_block.contains('1'), "{text}");
    Ok(())
}

/// The stdout block of a `bash` result's report.
pub(crate) fn stdout_block(output: &serde_json::Value) -> String {
    let text = output["content"][0]["text"].as_str().unwrap_or_default();
    text.split("<!-- start stdout -->")
        .nth(1)
        .and_then(|rest| rest.split("<!-- end stdout -->").next())
        .unwrap_or_else(|| panic!("stdout block in {text}"))
        .trim()
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim()
        .to_string()
}

/// A broker and a real `fs_read` tool server allowed to read `dir`, with
/// script mode configured over it. Dropping it stops the server.
struct FsReadServer {
    instance_id: ServerScope,
    config: Arc<ConfigLock>,
    _supervisor: ToolServerSupervisor,
    _env: EnvGuard,
    _broker: common::NatsServerHandle,
}

impl FsReadServer {
    /// `None` when the environment has no `nats-server`.
    async fn start(dir: &std::path::Path) -> Result<Option<Self>> {
        let Some(broker) = common::spawn_nats_server_with_options(common::SpawnNatsServerOptions {
            auth_token: Some(TOKEN.to_string()),
        })
        .await?
        else {
            return Ok(None);
        };
        let bridge = workspace_binary("harnx-mcp-bridge")?;
        let fs_tools = workspace_binary("harnx-fs-tools")?;
        let env = EnvGuard::install(&[
            (HARNX_NATS_URL_ENV, broker.url()),
            (HARNX_NATS_TOKEN_ENV, TOKEN),
        ]);
        let mut fs_server = time_server_config(&bridge.to_string_lossy());
        fs_server.name = "fs".to_string();
        fs_server.args = ["--name", "fs", "--"]
            .into_iter()
            .map(str::to_string)
            .chain([
                fs_tools.to_string_lossy().into_owned(),
                "--mcp-stdio".to_string(),
                "--allow-read".to_string(),
                dir.to_string_lossy().into_owned(),
            ])
            .collect();
        let (instance_id, supervisor, config) =
            start_script_server(broker.url(), fs_server, "__fs__fs", &["fs_read"]).await?;
        Ok(Some(Self {
            instance_id,
            config,
            _supervisor: supervisor,
            _env: env,
            _broker: broker,
        }))
    }
}

/// A file tool reached through the MCP bridge prints its shell form in a
/// script: the request for a structured result crosses the bridge and the
/// advertised mapping tells the script which field is stdout.
#[tokio::test(flavor = "multi_thread")]
async fn bridged_file_tool_prints_its_shell_form() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join("notes.txt");
    std::fs::write(&file, "alpha\nbeta\n")?;
    let Some(fs) = FsReadServer::start(dir.path()).await? else {
        return Ok(());
    };

    let script = format!("fs_read --path {} | head -1", file.display());
    let output = run_bash_script(&fs.config, &fs.instance_id, &script).await?;
    assert_eq!(output["status"], "ok", "{output}");
    assert_eq!(stdout_block(&output), "alpha", "{output}");
    Ok(())
}

/// Python in the `bash` tool calls a real file tool through `tools`: the
/// result's fields come back as a dict, and a read cut by `limit` carries
/// its note in `notes` instead of printing it.
#[tokio::test(flavor = "multi_thread")]
async fn python_reads_a_file_through_tools() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let short = dir.path().join("short.txt");
    let long = dir.path().join("long.txt");
    std::fs::write(&short, "alpha\nbeta\n")?;
    std::fs::write(&long, "one\ntwo\nthree\nfour\nfive\n")?;
    let Some(fs) = FsReadServer::start(dir.path()).await? else {
        return Ok(());
    };

    let script = format!(
        "python3 - <<'EOF'\nprint(tools.fs_read(path={short:?})['text'], end='')\nprint('--')\nprint(tools.fs_read(path={long:?}, limit=2)['notes'])\nEOF",
        short = short.display().to_string(),
        long = long.display().to_string(),
    );
    let output = run_bash_script(&fs.config, &fs.instance_id, &script).await?;
    assert_eq!(output["status"], "ok", "{output}");
    let stdout = stdout_block(&output).replace("\r\n", "\n");
    let (text, notes) = stdout.split_once("--\n").expect("separator in stdout");
    assert_eq!(text, "alpha\nbeta\n");
    assert!(notes.contains("offset"), "{notes}");
    Ok(())
}

/// A raw-JSON toolset answers BashKit identically natively and through the
/// stdio MCP bridge: `-o json` prints the tool's raw JSON, and Python's
/// `tools.*` returns the same dict. The bridge forwards the
/// structured-result request (`#2856`), and the MCP adapter honors it by
/// wrapping the raw JSON in `structuredContent` instead of a pretty-text
/// envelope (`#3004`).
#[tokio::test(flavor = "multi_thread")]
async fn nats_bridged_time_tool_matches_native_json_output() -> Result<()> {
    let Some(server) = common::spawn_nats_server_with_options(common::SpawnNatsServerOptions {
        auth_token: Some(TOKEN.to_string()),
    })
    .await?
    else {
        return Ok(());
    };
    let time = time_server_binary()?;
    let _env = EnvGuard::install(&[
        (HARNX_NATS_URL_ENV, server.url()),
        (HARNX_NATS_TOKEN_ENV, TOKEN),
    ]);

    let (native_id, _native, native_config) = start_script_server(
        server.url(),
        time_server_config(&time.to_string_lossy()),
        "__time__time",
        &["time_*"],
    )
    .await?;

    let mut bridged = time_server_config(&workspace_binary("harnx-mcp-bridge")?.to_string_lossy());
    bridged.args = ["--name", "time", "--"]
        .into_iter()
        .map(str::to_string)
        .chain([
            time.to_string_lossy().into_owned(),
            "--mcp-stdio".to_string(),
        ])
        .collect();
    let (bridge_id, _bridge, bridge_config) =
        start_script_server(server.url(), bridged, "__time__time", &["time_*"]).await?;

    // `-o json` prints the raw tool JSON on both wires. The whole object is
    // compared after dropping the two fields the clock moves between the
    // calls, so an MCP envelope (`content`/`isError`) on the bridged side
    // cannot pass as the tool's data.
    let script = "time_get_current_time --timezone UTC -o json";
    let native_json = run_bash_script(&native_config, &native_id, script).await?;
    assert_eq!(native_json["status"], "ok", "{native_json}");
    let bridged_json = run_bash_script(&bridge_config, &bridge_id, script).await?;
    assert_eq!(bridged_json["status"], "ok", "{bridged_json}");
    let native = stable_time_object(&stdout_block(&native_json))?;
    let bridged = stable_time_object(&stdout_block(&bridged_json))?;
    assert_eq!(
        bridged, native,
        "the bridged `-o json` output must be the raw tool JSON, not an MCP envelope"
    );

    // Python's `tools.*` returns the same dict on both wires.
    let script = r#"python3 - <<'EOF'
import json
d = tools.time_get_current_time(timezone="UTC")
print(json.dumps(sorted(d)))
EOF"#;
    let native_py = run_bash_script(&native_config, &native_id, script).await?;
    assert_eq!(native_py["status"], "ok", "{native_py}");
    let bridged_py = run_bash_script(&bridge_config, &bridge_id, script).await?;
    assert_eq!(bridged_py["status"], "ok", "{bridged_py}");
    let (native_keys, bridged_keys) = (stdout_block(&native_py), stdout_block(&bridged_py));
    assert_eq!(
        native_keys, bridged_keys,
        "native and bridged Python dicts must carry the same keys"
    );
    assert_eq!(
        bridged_keys, r#"["datetime", "day_of_week", "is_dst", "timezone"]"#,
        "the bridged dict must be the tool's raw JSON, not the envelope"
    );
    Ok(())
}

/// The `time_get_current_time` `-o json` output with the two fields the clock
/// moves between two calls removed. An MCP envelope or a text wrap parses to
/// an object with the wrong keys, so it fails the comparison, not just the
/// removed fields.
fn stable_time_object(stdout: &str) -> Result<serde_json::Value> {
    let mut value: serde_json::Value = serde_json::from_str(stdout)
        .with_context(|| format!("`-o json` output is one JSON object: {stdout}"))?;
    let object = value
        .as_object_mut()
        .with_context(|| format!("`-o json` output is one JSON object: {stdout}"))?;
    object.remove("datetime");
    object.remove("day_of_week");
    Ok(value)
}

/// Recovery: after worker loss the engine replays the call across providers
/// in order; an active BashProvider must answer `bash` with the interrupted
/// report so the script is never rerun (J-RHAD-r2-1). Active means the
/// agent's `bash_tools` actually resolve against registered declarations,
/// so this test stands up the real NATS broker and time
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
    let instance_id = ServerScope::new();
    let supervisor = start_time_server(
        server.url(),
        TOKEN,
        &instance_id,
        &[time_server_config(&binary)],
    )
    .await?;

    let config = script_mode_config(&["time_*"]);
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

/// When no declarations resolve (script mode inactive), even a bare `bash`
/// call must fall through replay untouched so a
/// tool-server-native `bash` keeps normal NATS journal recovery. The
/// provider-internal halves of both cases are covered in
/// `bash_provider::tests`; this pins the engine-level fall-through.
#[tokio::test(flavor = "multi_thread")]
async fn inactive_script_mode_leaves_bash_replay_to_other_providers() -> Result<()> {
    let config = script_mode_config(&["time_*"]);
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
    // qual:allow(dry, fragment) reason: "the replay e2es share the ctx-build + ToolCall fixture by design and diverge in what they assert about replay; deduplicating it would hide the replay policy under a fixture"
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

/// The fall-through above must actually reach NATS journal recovery.
/// Seeds the journal the way a completed tool-server-native
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
    seed_journaled_bash_reply(&client).await?;

    // No tool servers: script mode stays inactive, so the bare `bash` name
    // belongs to the journaled external tool.
    let config = script_mode_config(&["time_*"]);
    assert!(
        !config.read().bash_tools_active(),
        "no registered declarations means script mode is inactive"
    );
    let slot = Arc::new(BashEvalContextSlot::default());
    let instance_id = ServerScope::new();
    let mut params = BuildToolEvalContextParams::new(&config, &instance_id);
    params.bash_context_slot = Some(slot.clone());
    // qual:allow(dry, fragment) reason: "the replay e2es share the ctx-build + ToolCall fixture by design and diverge in what they assert about replay; deduplicating it would hide the replay policy under a fixture"
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

async fn seed_journaled_bash_reply(client: &async_nats::Client) -> Result<()> {
    let request = harnx_toolset::ToolRequest {
        user_id: None,
        input_responses: None,
        request_state: None,
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
                input_required: None,
                call_id: "wire-1".to_string(),
                result: Ok(json!({"native": true})),
                final_progress: None,
            },
        )
        .await?;
    Ok(())
}

/// `bash_exec` in a script behaves like a command: it prints the command's
/// stdout and stderr on the matching streams and exits with its status.
#[tokio::test(flavor = "multi_thread")]
async fn bash_exec_in_a_script_behaves_like_a_command() -> Result<()> {
    let Some(server) = common::spawn_nats_server_with_options(common::SpawnNatsServerOptions {
        auth_token: Some(TOKEN.to_string()),
    })
    .await?
    else {
        return Ok(());
    };
    let binary = workspace_binary("harnx-bash-tools")?;
    let dir = tempfile::tempdir()?;
    let _env = EnvGuard::install(&[
        (HARNX_NATS_URL_ENV, server.url()),
        (HARNX_NATS_TOKEN_ENV, TOKEN),
    ]);
    let mut bash_server = time_server_config(&binary.to_string_lossy());
    bash_server.name = "bash".to_string();
    bash_server.args = vec![
        "--no-sandbox".to_string(),
        "--allow-read".to_string(),
        dir.path().to_string_lossy().into_owned(),
    ];
    let (instance_id, supervisor, config) =
        start_script_server(server.url(), bash_server, "__bash__bash", &["bash_exec"]).await?;

    let script = r#"bash_exec --command 'echo hi' | tr a-z A-Z
bash_exec --command 'exit 3' || echo "status=$?"
bash_exec --command 'echo e >&2' 2>&1 >/dev/null
bash_exec --command 'echo hi' -o json | jq -r .exit_code"#;
    let output = run_bash_script(&config, &instance_id, script).await?;
    drop(supervisor);
    assert_eq!(output["status"], "ok", "{output}");
    assert_eq!(stdout_block(&output), "HI\nstatus=3\ne\n0", "{output}");
    Ok(())
}

/// The stderr block of a `bash` result's report.
fn stderr_block(output: &serde_json::Value) -> String {
    let text = output["content"][0]["text"].as_str().unwrap_or_default();
    text.split("<!-- start stderr -->")
        .nth(1)
        .and_then(|rest| rest.split("<!-- end stderr -->").next())
        .unwrap_or_default()
        .to_string()
}

/// A git repository with one committed file, with an identity in its own
/// config because the test sandbox strips the ambient one.
fn git_repository_with(file: &str, content: &str) -> Result<tempfile::TempDir> {
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join(file), content)?;
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.name", "Test"],
        vec!["config", "user.email", "test@example.com"],
        vec!["config", "commit.gpgsign", "false"],
        vec!["add", file],
        vec!["commit", "-q", "-m", "initial"],
    ] {
        let status = Command::new("git")
            .args(&args)
            .current_dir(dir.path())
            .status()?;
        anyhow::ensure!(status.success(), "git {args:?} failed");
    }
    Ok(dir)
}

/// The file and bash tool servers over one broker, both allowed to work in
/// `dir`, with `bash_tools` covering both toolsets.
async fn start_file_and_bash_servers(
    server_url: &str,
    dir: &std::path::Path,
) -> Result<(ServerScope, ToolServerSupervisor, Arc<ConfigLock>)> {
    let dir = dir.to_string_lossy().into_owned();
    let mut fs_server = time_server_config(&workspace_binary("harnx-fs-tools")?.to_string_lossy());
    fs_server.name = "fs".to_string();
    fs_server.args = vec!["--allow-write".into(), dir.clone()];
    let mut bash_server =
        time_server_config(&workspace_binary("harnx-bash-tools")?.to_string_lossy());
    bash_server.name = "bash".to_string();
    bash_server.args = vec!["--no-sandbox".into(), "--allow-rwx".into(), dir];
    let instance_id = ServerScope::new();
    let supervisor = start_tool_servers(
        server_url,
        TOKEN,
        &instance_id,
        &[fs_server, bash_server],
        &["__fs__fs", "__bash__bash"],
    )
    .await?;
    let config = script_mode_config(&["fs_*", "bash_*"]);
    harnx_runtime::tool_context::refresh_nats_tool_declarations(&config, &instance_id).await;
    Ok((instance_id, supervisor, config))
}

async fn assert_default_is_a_summary(
    config: &Arc<ConfigLock>,
    instance_id: &ServerScope,
    tracked: &std::path::Path,
) -> Result<()> {
    let script = format!("fs_write --path {} --content summary", quoted(tracked));
    let summary = run_bash_script(config, instance_id, &script).await?;
    let (stdout, stderr) = (stdout_block(&summary), stderr_block(&summary));
    assert!(stderr.contains(": git -C "), "{summary}");
    assert!(stderr.contains("1 file changed"), "{summary}");
    assert!(!stderr.contains("```diff"), "{summary}");
    assert!(
        !stdout.contains("git -C") && !stdout.is_empty(),
        "{summary}"
    );
    Ok(())
}

async fn assert_full_diff_goes_to_stderr(
    config: &Arc<ConfigLock>,
    instance_id: &ServerScope,
    tracked: &std::path::Path,
) -> Result<()> {
    let script = format!(
        "fs_write --path {} --content full --diff full",
        quoted(tracked)
    );
    let full = run_bash_script(config, instance_id, &script).await?;
    let (stdout, stderr) = (stdout_block(&full), stderr_block(&full));
    assert!(
        stderr.contains("```diff") && stderr.contains("+full"),
        "{full}"
    );
    assert!(
        !stdout.contains("```diff") && !stdout.contains("+full"),
        "{full}"
    );
    Ok(())
}

/// A path as one shell word, so a temp directory with a space in its name
/// still works.
fn quoted(path: &std::path::Path) -> String {
    shell_words::quote(&path.to_string_lossy()).into_owned()
}

/// The pinned-baseline snapshot, as the `bash` tool description teaches it.
const SNAPSHOT_BASE: &str = r#"base=$(bash_exec --working-dir "$d" --command 's=$(git stash create); echo ${s:-$(git rev-parse HEAD)}')"#;
const SHOW_CHANGES: &str =
    r#"bash_exec --working-dir "$d" --command "git status --short; git diff $base""#;

fn snapshot_recipe_script(repo: &std::path::Path, tracked: &std::path::Path) -> String {
    format!(
        "d={repo}\n{SNAPSHOT_BASE}\nfs_write --path {tracked} --content changed --diff none\nfs_write --path {fresh} --content brand-new --diff none\n{SHOW_CHANGES}",
        repo = quoted(repo),
        tracked = quoted(tracked),
        fresh = quoted(&repo.join("fresh.txt")),
    )
}

/// From a clean tree the snapshot is HEAD itself. The recipe commits one
/// change and leaves another uncommitted, so only a baseline pinned before
/// the commit shows both.
fn recipe_with_a_commit_script(repo: &std::path::Path, tracked: &std::path::Path) -> String {
    format!(
        "d={repo}\n{SNAPSHOT_BASE}\nfs_write --path {tracked} --content committed --diff none\nbash_exec --working-dir \"$d\" --command 'git commit -q -a -m committed'\nfs_write --path {second} --content uncommitted --diff none\n{SHOW_CHANGES}",
        repo = quoted(repo),
        tracked = quoted(tracked),
        second = quoted(&repo.join("second.txt")),
    )
}

fn commit_everything(repo: &std::path::Path) -> Result<()> {
    for args in [vec!["add", "-A"], vec!["commit", "-q", "-m", "settle"]] {
        let status = Command::new("git").args(&args).current_dir(repo).status()?;
        anyhow::ensure!(status.success(), "git {args:?} failed");
    }
    Ok(())
}

/// The earlier write left `full` in the tracked file, so a diff against the
/// snapshot removes `full`. A diff against HEAD would remove `old` instead,
/// which is what an empty snapshot id falls back to.
fn assert_recipe_diffs_against_the_snapshot(snapshot: &serde_json::Value) {
    let stdout = stdout_block(snapshot);
    assert!(stdout.contains("fresh.txt"), "{snapshot}");
    assert!(stdout.contains("+changed"), "{snapshot}");
    assert!(stdout.contains("-full"), "{snapshot}");
    assert!(!stdout.contains("-old"), "{snapshot}");
}

/// In a script a file write confirms on stdout and reports the change on
/// stderr: a one-line summary by default, the diff under `--diff full`.
/// A snapshot taken first (`git stash create`, or HEAD on a clean tree) gives one diff, with the
/// new file listed by `git status`, across several changes.
#[tokio::test(flavor = "multi_thread")]
async fn script_change_reports_and_snapshot_recipe() -> Result<()> {
    let Some(server) = common::spawn_nats_server_with_options(common::SpawnNatsServerOptions {
        auth_token: Some(TOKEN.to_string()),
    })
    .await?
    else {
        return Ok(());
    };
    let repo = git_repository_with("tracked.txt", "old\n")?;
    let _env = EnvGuard::install(&[
        (HARNX_NATS_URL_ENV, server.url()),
        (HARNX_NATS_TOKEN_ENV, TOKEN),
    ]);
    let (instance_id, supervisor, config) =
        start_file_and_bash_servers(server.url(), repo.path()).await?;
    let tracked = repo.path().join("tracked.txt");

    assert_default_is_a_summary(&config, &instance_id, &tracked).await?;
    assert_full_diff_goes_to_stderr(&config, &instance_id, &tracked).await?;
    let recipe = snapshot_recipe_script(repo.path(), &tracked);
    let snapshot = run_bash_script(&config, &instance_id, &recipe).await?;
    assert_recipe_diffs_against_the_snapshot(&snapshot);

    commit_everything(repo.path())?;
    let recipe = recipe_with_a_commit_script(repo.path(), &tracked);
    let committed = run_bash_script(&config, &instance_id, &recipe).await?;
    drop(supervisor);
    let stdout = stdout_block(&committed);
    assert!(stdout.contains("+committed"), "{committed}");
    assert!(stdout.contains("second.txt"), "{committed}");
    Ok(())
}

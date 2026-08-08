#[allow(dead_code)]
mod common;

use anyhow::{Context, Result};
use harnx_core::abort::create_abort_signal;
use harnx_core::agent_config::AgentConfig;
use harnx_core::instance::{ServerScope, HARNX_SERVER_SCOPE};
use harnx_core::tool::ToolCall;
use harnx_runtime::config::agent::Agent;
use harnx_runtime::config::{
    Config, GlobalConfig, ToolServerConfig, HARNX_NATS_TOKEN_ENV, HARNX_NATS_URL_ENV,
};
use harnx_runtime::nats_tool_provider::{NatsInFlightCalls, NatsToolProvider};
use harnx_runtime::nats_worker::{ToolServerStartConfig, ToolServerSupervisor};
use harnx_toolset::{server_identity_token, Registration};
use harnx_toolset_server::{registration_key, TOOL_REGISTRY_BUCKET};
use parking_lot::RwLock;
use serde_json::json;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

const TOKEN: &str = "package-tool-naming-e2e-token";
const SERVER_PACKAGE: &str = "tools-pkg";
const OTHER_PACKAGE: &str = "agent-pkg";

struct EnvGuard(Vec<(&'static str, Option<OsString>)>);

impl EnvGuard {
    fn install(values: &[(&'static str, &str)]) -> Self {
        let old = values
            .iter()
            .map(|(name, _)| (*name, std::env::var_os(name)))
            .collect();
        for (name, value) in values {
            // SAFETY: nextest runs this integration test in its own process, and
            // this file has one test, so no other thread mutates these variables.
            unsafe { std::env::set_var(name, value) };
        }
        Self(old)
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, value) in self.0.drain(..) {
            // SAFETY: see EnvGuard::install. Guard lives for the whole test.
            unsafe {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }
}

fn fs_server_binary() -> Result<PathBuf> {
    let mut path = std::env::current_exe().context("resolve test executable")?;
    path.pop();
    if path.file_name().is_some_and(|name| name == "deps") {
        path.pop();
    }
    path.push(if cfg!(windows) {
        "harnx-fs-tools.exe"
    } else {
        "harnx-fs-tools"
    });
    if path.is_file() {
        return Ok(path);
    }

    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .context("resolve workspace root")?
        .to_path_buf();
    let status = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .arg("build")
        .arg("-p")
        .arg("harnx-fs-tools")
        .current_dir(workspace)
        .status()
        .context("build harnx-fs-tools for package naming test")?;
    anyhow::ensure!(status.success(), "building harnx-fs-tools failed");
    anyhow::ensure!(
        path.is_file(),
        "harnx-fs-tools not found at {}",
        path.display()
    );
    Ok(path)
}

fn fs_server_config(binary: &Path, readable_dir: &Path) -> ToolServerConfig {
    ToolServerConfig {
        name: "fs".to_string(),
        command: binary.to_string_lossy().into_owned(),
        args: vec![
            "--allow-read".to_string(),
            readable_dir.to_string_lossy().into_owned(),
        ],
        env: Default::default(),
        enabled: true,
        description: None,
        package: Some(SERVER_PACKAGE.to_string()),
        hooks: None,
    }
}

fn agent(package: &str, tool_name: &str) -> AgentConfig {
    let mut agent = AgentConfig::from_prompt("");
    agent.set_name(&format!("{package}/reader"));
    agent.set_use_tools(Some(vec![tool_name.to_string()]));
    agent
}

fn config_for(agent_config: &AgentConfig) -> GlobalConfig {
    Arc::new(RwLock::new(Config {
        agent: Some(Agent::new(agent_config.clone())),
        ..Config::default()
    }))
}

async fn wait_for_fs_registration(
    client: &async_nats::Client,
    instance_id: &ServerScope,
) -> Result<Registration> {
    let store = async_nats::jetstream::new(client.clone())
        .get_key_value(TOOL_REGISTRY_BUCKET)
        .await?;
    let identity = server_identity_token(Some(SERVER_PACKAGE), "fs", "fs");
    let key = registration_key(instance_id, &identity);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(entry) = store.get(&key).await? {
            return serde_json::from_slice(&entry).context("decode fs registration");
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "fs registration {key} did not appear"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn assert_schema_contains(
    config: &GlobalConfig,
    instance_id: &ServerScope,
    agent_config: &AgentConfig,
    expected_name: &str,
) {
    harnx_runtime::tool_context::refresh_nats_tool_declarations(config, instance_id).await;
    let declarations = config.read().select_tools(agent_config).unwrap_or_default();
    assert!(
        declarations.iter().any(|tool| tool.name == expected_name),
        "selected schema did not contain {expected_name}: {:?}",
        declarations
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>()
    );
}

struct ReadAssertion<'a> {
    config: &'a GlobalConfig,
    instance_id: &'a ServerScope,
    package: &'a str,
    visible_name: &'a str,
    file: &'a Path,
    expected_content: &'a str,
}

async fn assert_read_executes(params: ReadAssertion<'_>) -> Result<()> {
    let context = harnx_runtime::tool::build_tool_eval_context(
        harnx_runtime::tool::BuildToolEvalContextParams::new(params.config, params.instance_id)
            .with_agent_use_tools(Some(params.visible_name))
            .with_current_agent_package(Some(params.package.to_string())),
    )
    .await;
    let call_id = format!("{}-fs-read", params.package);
    let results = harnx_runtime::tool::eval_tool_calls(
        &context,
        vec![ToolCall::new(
            params.visible_name.to_string(),
            json!({ "path": params.file }),
            Some(call_id.clone()),
            None,
        )],
        &create_abort_signal(),
    )
    .await?;
    let result = results
        .iter()
        .find(|result| result.call.id.as_deref() == Some(call_id.as_str()))
        .context("fs result in worker transcript")?;
    assert!(
        result.output.get("error").is_none(),
        "fs read returned an error: {}",
        result.output
    );
    assert!(
        result.output.to_string().contains(params.expected_content),
        "fs read result did not contain file content: {}",
        result.output
    );
    Ok(())
}

struct E2eServer {
    _nats: common::NatsServerHandle,
    _temp: tempfile::TempDir,
    _env: EnvGuard,
    _supervisor: ToolServerSupervisor,
    instance_id: ServerScope,
    file: PathBuf,
    expected_content: &'static str,
}

async fn start_e2e_server() -> Result<Option<E2eServer>> {
    let Some(nats) = common::spawn_nats_server_with_options(common::SpawnNatsServerOptions {
        auth_token: Some(TOKEN.to_string()),
    })
    .await?
    else {
        return Ok(None);
    };
    let temp = tempfile::tempdir()?;
    let file = temp.path().join("proof.txt");
    let expected_content = "package-aware fs read succeeded";
    std::fs::write(&file, expected_content)?;
    let binary = fs_server_binary()?;
    let client = async_nats::ConnectOptions::new()
        .token(TOKEN.to_string())
        .connect(nats.url())
        .await?;
    let instance_id = ServerScope::new();
    let env = EnvGuard::install(&[
        (HARNX_NATS_URL_ENV, nats.url()),
        (HARNX_NATS_TOKEN_ENV, TOKEN),
        (HARNX_SERVER_SCOPE, instance_id.as_str()),
    ]);
    let start = ToolServerStartConfig::new(client.clone(), instance_id.clone(), nats.url(), TOKEN);
    let servers = [fs_server_config(&binary, temp.path())];
    let supervisor =
        ToolServerSupervisor::start_local_with_timeout(start, &servers, Duration::from_secs(5))
            .await?;
    let registration = wait_for_fs_registration(&client, &instance_id).await?;
    assert_eq!(registration.package.as_deref(), Some(SERVER_PACKAGE));
    assert_eq!(registration.config, "fs");
    assert_eq!(registration.server, "fs");
    assert!(registration.tools.iter().any(|tool| tool.name == "read"));

    Ok(Some(E2eServer {
        _nats: nats,
        _temp: temp,
        _env: env,
        _supervisor: supervisor,
        instance_id,
        file,
        expected_content,
    }))
}

async fn assert_schema_and_execution(
    server: &E2eServer,
    agent_package: &str,
    visible_name: &str,
) -> Result<()> {
    let agent = agent(agent_package, visible_name);
    let config = config_for(&agent);
    assert_eq!(
        config.read().active_package().as_deref(),
        Some(agent_package)
    );
    let config_snapshot = config.read().clone();
    let discovered = NatsToolProvider::discover(
        &config_snapshot,
        server.instance_id.clone(),
        NatsInFlightCalls::for_instance(&server.instance_id),
        Some(agent_package),
    )
    .await?;
    let discovered_names = discovered
        .declarations_for_use_tools(Some("*"))
        .into_iter()
        .map(|tool| tool.name)
        .collect::<Vec<_>>();
    assert!(
        discovered_names.iter().any(|tool| tool == visible_name),
        "live provider did not expose {visible_name}: {discovered_names:?}"
    );
    assert_schema_contains(&config, &server.instance_id, &agent, visible_name).await;
    assert_read_executes(ReadAssertion {
        config: &config,
        instance_id: &server.instance_id,
        package: agent_package,
        visible_name,
        file: &server.file,
        expected_content: server.expected_content,
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn model_schema_selection_routing_and_execution_use_package_tool_names() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = start_e2e_server().await? else {
        return Ok(());
    };

    assert_schema_and_execution(&server, SERVER_PACKAGE, "fs_read").await?;
    let cross_package_name = format!("{SERVER_PACKAGE}__fs_read");
    assert_schema_and_execution(&server, OTHER_PACKAGE, &cross_package_name).await?;

    // Distinct identities for same-named packaged servers are covered by
    // tool_supervisor::same_named_packaged_servers_use_distinct_identities_and_cleanup (#1350).
    Ok(())
}

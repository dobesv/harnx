//! Shipped package → in-process middleware → real NATS proxy/bash services.
#![cfg(unix)]

use crate::common::{spawn_nats_server_with_options, SpawnNatsServerOptions};
use anyhow::{Context, Result};
use harnx_core::{
    abort::create_abort_signal,
    instance::{ServerScope, HARNX_SERVER_SCOPE},
    tool::{ToolCall, ToolProvider},
};
use harnx_engine::tool::{eval_tool_calls, ToolEvalContext};
use harnx_runtime::{
    config::{Config, ConfigLock, GlobalConfig, ToolServerConfig},
    nats_tool_provider::NatsToolProvider,
    nats_worker::{ToolServerStartConfig, ToolServerSupervisor},
    tool::{build_tool_eval_context_with_provider, BuildToolEvalContextParams},
};
use serde_json::{json, Value};
use std::{path::Path, sync::Arc, time::Duration};
use tokio::process::{Child, Command};

use super::publish_plan::{check_installed, github_fixture_hook, install_publisher_fixture};

pub(super) const TOKEN: &str = "proxy-middleware-test";
const STARTUP: Duration = Duration::from_secs(20);
const CLIENT: &str = include_str!("proxy_auth_client.py");
const HOST_CONFIG: &str = "version: 1\ncurrent_profile: cloud:account\nprofiles:\n    - site: example.atlassian.net\n      cloud_id: cloud\n      account_id: account\n      email: test@example.invalid\n      auth_type: api_token\n";
// Terminal stage echoes transformed headers without making upstream requests.
const ECHO: &str =
    "if .method != \"CONNECT\" then .respond = {status: 200, body: (.headers | tojson)} end";

// qual:allow(unsafe) reason: "Nextest isolates this process; config loading and NATS discovery read environment variables."
pub(super) fn configure_environment(root: &Path, url: &str, scope: &ServerScope) {
    harnx_core::require_nextest();
    // SAFETY: nextest isolates each test in its own process. No concurrent tests
    // read this environment, and child services get explicit fixture settings.
    unsafe {
        std::env::set_var("HARNX_CONFIG_DIR", root);
        std::env::set_var("HARNX_NATS_URL", url);
        std::env::set_var("HARNX_NATS_TOKEN", TOKEN);
        std::env::set_var(HARNX_SERVER_SCOPE, scope.as_str());
    }
}

fn shipped_config(root: &Path, package: &str) -> Result<Config> {
    let packages = root.join("packages");
    std::fs::create_dir_all(&packages)?;
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages")
        .join(package);
    std::os::unix::fs::symlink(source, packages.join(package))?;
    let path = root.join("config.yaml");
    std::fs::write(&path, "use_tools: [bash_exec, bash_spawn, bash_wait]\n")?;
    let mut config = Config::load_from_file(&path)?;
    config.tool_servers.retain(|s| {
        s.package.as_deref() == Some(package) && matches!(s.name.as_str(), "bash" | "proxy_auth")
    });
    assert_eq!(config.tool_servers.len(), 2);
    let host_config = root.join("jira_config.yaml");
    std::fs::write(&host_config, HOST_CONFIG)?;
    for server in &mut config.tool_servers {
        assert!(
            server.hooks.is_none(),
            "shipped proxy auth must not use hooks"
        );
        server.command = binary(&server.command).to_string_lossy().into_owned();
        if server.name == "bash" {
            // This fixture tests middleware/transport, not nested OS sandboxing.
            // The outer nextest runner already owns isolation; some CI hosts
            // cannot create the second sandbox required by the bash service.
            server.args.push("--no-sandbox".into());
        }
        if server.name == "proxy_auth" {
            server.args.extend(["--hook".into(), ECHO.into()]);
            server.env.extend([
                ("GITHUB_TOKEN".into(), "fixture-github-token".into()),
                (
                    "HARNX_JIRA_TOKEN_CMD".into(),
                    "printf fixture-jira-token".into(),
                ),
                (
                    "ACLI_HOST_CONFIG".into(),
                    host_config.to_string_lossy().into_owned(),
                ),
            ]);
        }
    }
    assert_eq!(config.resolved_middleware()?.entries.len(), 1);
    Ok(config)
}

/// The registered `bash_publish_plan` tool's installed-package fixture: the
/// shipped bash config with the publisher template installed, no global
/// script/env grant, and the proxy service's synthetic-GitHub hook.
fn publisher_config(root: &Path, package: &str) -> Result<Config> {
    let packages = root.join("packages");
    std::fs::create_dir_all(&packages)?;
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages")
        .join(package);
    let installed = packages.join(package);
    install_publisher_fixture(&source, &installed)?;
    let path = root.join("config.yaml");
    std::fs::write(
        &path,
        "use_tools: [bash_exec, bash_spawn, bash_wait, bash_publish_plan]\n",
    )?;
    let mut config = Config::load_from_file(&path)?;
    config.tool_servers.retain(|s| {
        s.package.as_deref() == Some(package) && matches!(s.name.as_str(), "bash" | "proxy_auth")
    });
    assert_eq!(config.tool_servers.len(), 2);
    let host_config = root.join("jira_config.yaml");
    std::fs::write(&host_config, HOST_CONFIG)?;
    for server in &mut config.tool_servers {
        assert!(
            server.hooks.is_none(),
            "shipped proxy auth must not use hooks"
        );
        server.command = binary(&server.command).to_string_lossy().into_owned();
        if server.name == "bash" {
            // The registered publish tool carries its own script grant; the
            // global Bash script-path and HARNX_PACKAGE_DIR grants the
            // branch used to add must stay gone.
            assert!(!server.args.windows(2).any(|args| args
                == [
                    "--allow-read",
                    "~/.config/harnx/packages/pantheon/scripts/publish-plan.sh"
                ]));
            assert!(!server
                .args
                .windows(2)
                .any(|args| args == ["--env", "HARNX_PACKAGE_DIR"]));
            // The publisher finds the fixture curl through the server's own
            // PATH; a typed tool call carries no caller env.
            let path = std::env::var("PATH").unwrap_or_default();
            server.env.insert(
                "PATH".into(),
                format!("{}:{path}", root.join("bin").to_string_lossy()),
            );
            // This fixture tests the ambient credential route, not nested OS
            // sandboxing. The outer nextest runner already owns isolation;
            // some CI hosts cannot create the second sandbox required by the
            // bash service.
            server.args.push("--no-sandbox".into());
        }
        if server.name == "proxy_auth" {
            // Synthetic GitHub: the hook answers api.github.com and
            // github.com requests that carry the fixture token, standing in
            // for the real GitHub HTTPS boundary.
            server.args.extend(["--hook".into(), github_fixture_hook()]);
            server.env.extend([
                ("GITHUB_TOKEN".into(), "fixture-github-token".into()),
                (
                    "HARNX_JIRA_TOKEN_CMD".into(),
                    "printf fixture-jira-token".into(),
                ),
                (
                    "ACLI_HOST_CONFIG".into(),
                    host_config.to_string_lossy().into_owned(),
                ),
            ]);
        }
    }
    assert_eq!(config.resolved_middleware()?.entries.len(), 1);
    Ok(config)
}

pub(super) fn binary(name: &str) -> std::path::PathBuf {
    let path = harnx_core::bin_dir_of_exe(&std::env::current_exe().unwrap()).join(name);
    assert!(
        path.is_file(),
        "build {name} before running proxy middleware tests: {}",
        path.display()
    );
    path
}

pub(super) fn deploy_independently(
    server: &ToolServerConfig,
    root: &Path,
    scope: &ServerScope,
    url: &str,
) -> Result<Child> {
    let mut command = Command::new(&server.command);
    command
        .args(&server.args)
        .args(["--name", &server.name])
        .envs(&server.env)
        .env("HARNX_NATS_URL", url)
        .env("HARNX_NATS_TOKEN", TOKEN)
        .env(HARNX_SERVER_SCOPE, scope.as_str())
        .env("HARNX_SERVER_CONFIG", &server.name)
        .env("HARNX_SERVER_PACKAGE", server.package.as_deref().unwrap())
        .env(
            "HARNX_PACKAGE_DIR",
            root.join("packages")
                .join(server.package.as_deref().unwrap()),
        )
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true);
    command.spawn().context("deploy independent tool server")
}

async fn discover(config: &Config, scope: &ServerScope, package: &str) -> Arc<NatsToolProvider> {
    tokio::time::timeout(STARTUP, async {
        loop {
            if let Ok(provider) =
                NatsToolProvider::discover(config, scope.clone(), Default::default(), Some(package))
                    .await
            {
                if provider.has_tool("bash_exec")
                    && provider.has_tool("bash_spawn")
                    && provider.has_tool("proxy_auth_environment")
                {
                    return Arc::new(provider);
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("both proxy/bash tool servers register")
}

pub(super) async fn invoke(ctx: &ToolEvalContext, name: &str, args: Value) -> Value {
    let result = Box::pin(eval_tool_calls(
        ctx,
        vec![ToolCall::new(
            name.into(),
            args,
            Some(uuid::Uuid::new_v4().to_string()),
            None,
        )],
        &create_abort_signal(),
    ))
    .await
    .unwrap()
    .remove(0);
    assert_ne!(result.output["is_error"], true, "{}", result.output);
    assert_ne!(result.output["isError"], true, "{}", result.output);
    // Bash tools return the same human-readable envelope over NATS and MCP.
    let text = result.output["content"][0]["text"].as_str().unwrap();
    let yaml = text
        .strip_prefix("```yaml\n")
        .unwrap()
        .split("\n```")
        .next()
        .unwrap();
    let mut output: Value = serde_yaml::from_str(yaml).unwrap();
    let stdout = text
        .split("<!-- start stdout -->\n```\n")
        .nth(1)
        .unwrap()
        .split("\n```\n<!-- end stdout -->")
        .next()
        .unwrap();
    output["stdout"] = json!(stdout);
    output["diagnostic"] = json!(text);
    output
}

async fn check_exec_and_spawn(ctx: &ToolEvalContext) {
    for name in ["bash_exec", "bash_spawn"] {
        let mut args = json!({"command": CLIENT, "env": {"CALLER_VALUE":"kept"}});
        if name == "bash_spawn" {
            args["wait"] = json!({"seconds":30});
        }
        let mut result = invoke(ctx, name, args).await;
        if name == "bash_spawn" && result["status"] == "running" {
            result = invoke(
                ctx,
                "bash_wait",
                json!({"execution_id":result["execution_id"],"seconds":30}),
            )
            .await;
        }
        assert_eq!(result["exit_code"], 0, "{name}: {result}");
        assert!(
            result["stdout"]
                .as_str()
                .unwrap()
                .contains("PROXY_MIDDLEWARE_OK"),
            "{name}: {result}"
        );
        let result = invoke(ctx, name, json!({
            "command":"printf '%s' \"$HTTPS_PROXY|$SSL_CERT_FILE|$CALLER_VALUE\"",
            "env":{"HTTPS_PROXY":"caller-proxy", "SSL_CERT_FILE":"caller-ca", "CALLER_VALUE":"kept"},
            "wait": if name == "bash_spawn" { json!({"seconds":30}) } else { Value::Null }
        })).await;
        assert_eq!(result["exit_code"], 0, "{name}: {result}");
        assert!(
            result["stdout"]
                .as_str()
                .unwrap()
                .contains("caller-proxy|caller-ca|kept"),
            "{name}: {result}"
        );
    }
}

async fn run(package: &str, managed: bool) -> Result<()> {
    harnx_core::require_nextest();
    let Some(nats) = spawn_nats_server_with_options(SpawnNatsServerOptions {
        auth_token: Some(TOKEN.into()),
    })
    .await?
    else {
        return Ok(());
    };
    let root = tempfile::tempdir()?;
    let scope = ServerScope::new();
    configure_environment(root.path(), nats.url(), &scope);
    let config = shipped_config(root.path(), package)?;
    let mut independent = Vec::new();
    let supervisor = if managed {
        let client = async_nats::ConnectOptions::new()
            .token(TOKEN.into())
            .connect(nats.url())
            .await?;
        let supervisor = ToolServerSupervisor::start_local(
            ToolServerStartConfig::new(client, scope.clone(), nats.url(), TOKEN)
                .inheriting_child_output(),
            &config.tool_servers,
        )
        .await?;
        assert!(supervisor.unregistered_servers().is_empty());
        Some(supervisor)
    } else {
        for server in &config.tool_servers {
            independent.push(deploy_independently(
                server,
                root.path(),
                &scope,
                nats.url(),
            )?);
        }
        None
    };
    let provider = discover(&config, &scope, package).await;
    *config.nats_tool_declarations.write() = provider.declarations().to_vec();
    let config: GlobalConfig = Arc::new(ConfigLock::new(config));
    let mut params = BuildToolEvalContextParams::new(&config, &scope);
    params.current_agent_package = Some(package.into());
    let ctx = Box::pin(build_tool_eval_context_with_provider(params, provider)).await;
    Box::pin(check_exec_and_spawn(&ctx)).await;
    if let Some(supervisor) = supervisor {
        supervisor.shutdown().await;
    }
    for mut child in independent {
        child.kill().await?;
    }
    Ok(())
}

/// Waits until a provider for the scope offers the tools `ready` expects.
async fn discover_until(
    config: &Config,
    scope: &ServerScope,
    package: &str,
    ready: impl Fn(&NatsToolProvider) -> bool,
) -> Arc<NatsToolProvider> {
    tokio::time::timeout(STARTUP, async {
        loop {
            if let Ok(provider) =
                NatsToolProvider::discover(config, scope.clone(), Default::default(), Some(package))
                    .await
            {
                if ready(&provider) {
                    return Arc::new(provider);
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("fixture tool servers register")
}

/// The registered `bash_publish_plan` tool runs with no middleware: the
/// publisher authenticates through the ambient GitHub credential route, the
/// same one wait_for_pr_stable uses. The proxy service here is only the
/// synthetic GitHub that ambient egress configuration points the
/// authenticated route at, so it must start first: its address and CA travel
/// in the bash server's ambient environment, the way a real ambient egress
/// proxy would.
async fn run_publisher(package: &str) -> Result<()> {
    harnx_core::require_nextest();
    let Some(nats) = spawn_nats_server_with_options(SpawnNatsServerOptions {
        auth_token: Some(TOKEN.into()),
    })
    .await?
    else {
        return Ok(());
    };
    let root = tempfile::tempdir()?;
    let scope = ServerScope::new();
    configure_environment(root.path(), nats.url(), &scope);
    let config = publisher_config(root.path(), package)?;
    let client = async_nats::ConnectOptions::new()
        .token(TOKEN.into())
        .connect(nats.url())
        .await?;

    // Phase 1: the synthetic-GitHub proxy service alone, so its egress
    // environment can be resolved before the bash server starts.
    let proxy_server = config
        .tool_servers
        .iter()
        .find(|server| server.name == "proxy_auth")
        .cloned()
        .expect("proxy_auth server config");
    let proxy_supervisor = ToolServerSupervisor::start_local(
        ToolServerStartConfig::new(client.clone(), scope.clone(), nats.url(), TOKEN)
            .inheriting_child_output(),
        &[proxy_server],
    )
    .await?;
    assert!(proxy_supervisor.unregistered_servers().is_empty());
    let provider = discover_until(&config, &scope, package, |provider| {
        provider.has_tool("proxy_auth_environment")
    })
    .await;
    let environment = match provider
        .call_tool("proxy_auth_environment", json!({}), &create_abort_signal())
        .await
    {
        Ok(output) => output.value,
        Err(_) => panic!("proxy environment lookup failed"),
    };
    let environment = match environment.get("content") {
        Some(content) => serde_json::from_str::<Value>(
            content[0]["text"].as_str().expect("environment JSON text"),
        )
        .expect("parse proxy environment JSON"),
        None => environment,
    };
    let egress = environment.get("env").cloned().unwrap_or_else(|| json!({}));

    // Phase 2: the bash server with the waiter-style credential route. The
    // token rides as declared ambient passthrough: the template declares
    // GITHUB_TOKEN in sandbox env, so the invocation resolves it from the
    // server process environment. The egress configuration rides as
    // name-only `--env NAME` passthrough — the existing deployment
    // mechanism for ambient host values — so the publisher's script finds
    // proxy and CA settings in its own environment, exactly like the waiter.
    // The typed publish call itself carries none of it.
    let mut bash_server = config
        .tool_servers
        .iter()
        .find(|server| server.name == "bash")
        .cloned()
        .expect("bash server config");
    bash_server
        .env
        .insert("GITHUB_TOKEN".into(), "fixture-github-token".into());
    for key in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "no_proxy",
        "SSL_CERT_FILE",
        "CURL_CA_BUNDLE",
        "GIT_SSL_CAINFO",
    ] {
        if let Some(value) = egress.get(key).and_then(Value::as_str) {
            bash_server.env.insert(key.into(), value.to_string());
            bash_server.args.extend(["--env".into(), key.into()]);
        }
    }
    let bash_supervisor = ToolServerSupervisor::start_local(
        ToolServerStartConfig::new(client, scope.clone(), nats.url(), TOKEN)
            .inheriting_child_output(),
        &[bash_server],
    )
    .await?;
    assert!(bash_supervisor.unregistered_servers().is_empty());

    let provider = discover_until(&config, &scope, package, |provider| {
        provider.has_tool("bash_publish_plan") && provider.has_tool("proxy_auth_environment")
    })
    .await;
    *config.nats_tool_declarations.write() = provider.declarations().to_vec();
    let config: GlobalConfig = Arc::new(ConfigLock::new(config));
    let mut params = BuildToolEvalContextParams::new(&config, &scope);
    params.current_agent_package = Some(package.into());
    let ctx = Box::pin(build_tool_eval_context_with_provider(params, provider)).await;
    Box::pin(check_installed(&ctx, root.path())).await?;
    bash_supervisor.shutdown().await;
    proxy_supervisor.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn publish_plan_installed_uses_ambient_github_credentials() -> Result<()> {
    Box::pin(run_publisher("pantheon")).await
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_proxy_pantheon_managed() -> Result<()> {
    Box::pin(run("pantheon", true)).await
}
#[tokio::test(flavor = "multi_thread")]
async fn middleware_proxy_pantheon_unmanaged() -> Result<()> {
    Box::pin(run("pantheon", false)).await
}
#[tokio::test(flavor = "multi_thread")]
async fn middleware_proxy_coding_managed() -> Result<()> {
    Box::pin(run("coding", true)).await
}
#[tokio::test(flavor = "multi_thread")]
async fn middleware_proxy_coding_unmanaged() -> Result<()> {
    Box::pin(run("coding", false)).await
}

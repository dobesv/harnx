//! Documented credential package → worker middleware → real credential/bash services.
#![cfg(unix)]

use super::proxy_auth::{binary, configure_environment, deploy_independently, invoke, TOKEN};
use crate::common::{spawn_nats_server_with_options, SpawnNatsServerOptions};
use anyhow::Result;
use harnx_core::{instance::ServerScope, tool::ToolProvider};
use harnx_engine::tool::ToolEvalContext;
use harnx_runtime::{
    config::{Config, ConfigLock, GlobalConfig, ToolServerConfig},
    nats_tool_provider::NatsToolProvider,
    nats_worker::{ToolServerStartConfig, ToolServerSupervisor},
    tool::{build_tool_eval_context_with_provider, BuildToolEvalContextParams},
};
use serde_json::{json, Value};
use std::{path::Path, sync::Arc, time::Duration};

const PACKAGE: &str = "credential-tools";
const CLIENT: &str = include_str!("credentials_client.py");
const PROCESS: &str = include_str!("credential_process.py");
const STARTUP: Duration = Duration::from_secs(20);

fn host_credentials(root: &Path) -> Result<()> {
    std::fs::write(root.join("credential_process.py"), PROCESS)?;
    std::fs::write(root.join("empty-credentials"), "")?;
    let python = which::which("python3")?;
    let process = root.join("credential_process.py");
    let aws_counter = root.join("aws-counter");
    let command = shell_words::join([
        python.to_string_lossy().as_ref(),
        process.to_string_lossy().as_ref(),
        "aws",
        aws_counter.to_string_lossy().as_ref(),
    ]);
    std::fs::write(
        root.join("aws-config"),
        format!("[profile fixture]\nregion = us-west-2\ncredential_process = {command}\n"),
    )?;
    let kubeconfig = json!({
        "apiVersion":"v1", "kind":"Config", "current-context":"my-cluster",
        "clusters":[{"name":"fixture", "cluster":{"server":"https://127.0.0.1:6443"}}],
        "contexts":[{"name":"my-cluster", "context":{"cluster":"fixture","user":"fixture"}}],
        "users":[{"name":"fixture", "user":{"exec":{
            "apiVersion":"client.authentication.k8s.io/v1", "command":python,
            "args":[process,"k8s",root.join("k8s-counter")], "interactiveMode":"Never",
        }}}],
    });
    std::fs::write(
        root.join("host-kubeconfig"),
        serde_yaml::to_string(&kubeconfig)?,
    )?;
    Ok(())
}

fn configure_service(server: &mut ToolServerConfig, root: &Path) {
    server.command = binary(&server.command).to_string_lossy().into_owned();
    assert!(
        server.hooks.is_none(),
        "documented config must not use legacy hooks"
    );
    match server.name.as_str() {
        "bash" => {
            // Same fixture constraint as proxy_auth: outer nextest isolation is
            // active, but this host cannot create a second nested OS sandbox.
            server.args.push("--no-sandbox".into());
        }
        "aws_creds" => {
            server.env.extend([
                (
                    "AWS_CONFIG_FILE".into(),
                    root.join("aws-config").to_string_lossy().into_owned(),
                ),
                (
                    "AWS_SHARED_CREDENTIALS_FILE".into(),
                    root.join("empty-credentials")
                        .to_string_lossy()
                        .into_owned(),
                ),
                ("AWS_PROFILE".into(), "fixture".into()),
                ("AWS_REGION".into(), "us-west-2".into()),
                ("AWS_EC2_METADATA_DISABLED".into(), "true".into()),
            ]);
        }
        "k8s_creds" => server.args.extend([
            "--kubeconfig".into(),
            root.join("host-kubeconfig").to_string_lossy().into_owned(),
        ]),
        other => panic!("unexpected credential example service: {other}"),
    }
}

fn documented_config(root: &Path) -> Result<Config> {
    host_credentials(root)?;
    std::fs::create_dir_all(root.join("packages"))?;
    let source =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../example_config/credential-tools");
    std::os::unix::fs::symlink(source, root.join("packages").join(PACKAGE))?;
    let path = root.join("config.yaml");
    std::fs::write(&path, "use_tools: [bash_exec, bash_spawn, bash_wait]\n")?;
    let mut config = Config::load_from_file(&path)?;
    config
        .tool_servers
        .retain(|s| s.package.as_deref() == Some(PACKAGE));
    assert_eq!(config.tool_servers.len(), 3);
    for server in &mut config.tool_servers {
        configure_service(server, root);
    }
    assert_eq!(config.resolved_middleware()?.entries.len(), 2);
    Ok(config)
}

async fn discover(config: &Config, scope: &ServerScope) -> Arc<NatsToolProvider> {
    tokio::time::timeout(STARTUP, async {
        loop {
            if let Ok(provider) =
                NatsToolProvider::discover(config, scope.clone(), Default::default(), Some(PACKAGE))
                    .await
            {
                if [
                    "bash_exec",
                    "bash_spawn",
                    "aws_creds_environment",
                    "k8s_creds_environment",
                ]
                .iter()
                .all(|tool| provider.has_tool(tool))
                {
                    return Arc::new(provider);
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("credential/bash services register")
}

async fn client(ctx: &ToolEvalContext, tool: &str, kind: &str) -> Value {
    let mut args = json!({"command":CLIENT,"env":{
        "CALLER_VALUE":"kept", "CREDENTIAL_KIND":kind,
        "AWS_REGION":"caller-region", "AWS_CONTAINER_CREDENTIALS_FULL_URI":"caller-uri",
        "AWS_CONTAINER_AUTHORIZATION_TOKEN":"caller-token", "KUBECONFIG":"caller-kubeconfig",
    }});
    if tool == "bash_spawn" {
        args["wait"] = json!({"seconds":30});
    }
    let mut output = invoke(ctx, tool, args).await;
    if output["status"] == "running" {
        output = invoke(
            ctx,
            "bash_wait",
            json!({"execution_id":output["execution_id"],"seconds":30}),
        )
        .await;
    }
    assert_eq!(output["exit_code"], 0, "{tool}/{kind}: {output}");
    serde_json::from_str(output["stdout"].as_str().unwrap().trim()).unwrap()
}

async fn wait_for_expiry(reply: &Value) {
    let expiry =
        chrono::DateTime::parse_from_rfc3339(reply["expiration"].as_str().unwrap()).unwrap();
    let delay = (expiry.with_timezone(&chrono::Utc) - chrono::Utc::now())
        .to_std()
        .unwrap_or_default();
    assert!(
        delay < Duration::from_secs(3),
        "fixture returned unexpected long expiry"
    );
    tokio::time::sleep(delay + Duration::from_millis(50)).await;
}

async fn check_refresh(ctx: &ToolEvalContext, root: &Path) {
    for kind in ["aws", "k8s"] {
        let first = client(ctx, "bash_exec", kind).await;
        assert_eq!(first["generation"], 1, "{kind}: {first}");
        if kind == "aws" {
            wait_for_expiry(&first).await;
        }
        let second = client(ctx, "bash_spawn", kind).await;
        assert_eq!(
            second["generation"], 2,
            "later {kind} call must refresh: {second}"
        );
        let third = client(ctx, "bash_exec", kind).await;
        // AWS forwards every request to its provider. K8s caches until the
        // existing 60-second refresh margin; don't impose AWS caching here.
        assert_eq!(third["generation"], if kind == "aws" { 3 } else { 2 });
        assert_eq!(
            first["path"], third["path"],
            "synthetic config stays provisioned across calls"
        );
        if kind == "k8s" {
            let path = Path::new(first["path"].as_str().unwrap());
            let config: Value =
                serde_yaml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
            assert_eq!(config["users"][0]["user"]["exec"]["command"], "curl");
            assert_eq!(config["current-context"], "my-cluster");
            assert_ne!(path, root.join("host-kubeconfig"));
        }
    }
}

async fn run(managed: bool) -> Result<()> {
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
    let config = documented_config(root.path())?;
    let mut independent = vec![];
    let supervisor = if managed {
        let client = async_nats::ConnectOptions::new()
            .token(TOKEN.into())
            .connect(nats.url())
            .await?;
        Some(
            ToolServerSupervisor::start_local(
                ToolServerStartConfig::new(client, scope.clone(), nats.url(), TOKEN)
                    .inheriting_child_output(),
                &config.tool_servers,
            )
            .await?,
        )
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
    let provider = discover(&config, &scope).await;
    *config.nats_tool_declarations.write() = provider.declarations().to_vec();
    let config: GlobalConfig = Arc::new(ConfigLock::new(config));
    let mut params = BuildToolEvalContextParams::new(&config, &scope);
    params.current_agent_package = Some(PACKAGE.into());
    let ctx = Box::pin(build_tool_eval_context_with_provider(params, provider)).await;
    Box::pin(check_refresh(&ctx, root.path())).await;
    if let Some(supervisor) = supervisor {
        supervisor.shutdown().await;
    }
    for mut child in independent {
        child.kill().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_credentials_managed_exec_spawn_refresh() -> Result<()> {
    Box::pin(run(true)).await
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_credentials_unmanaged_exec_spawn_refresh() -> Result<()> {
    Box::pin(run(false)).await
}

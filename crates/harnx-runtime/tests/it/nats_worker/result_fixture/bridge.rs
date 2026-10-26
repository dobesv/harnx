use super::*;
use harnx_core::child_process::ChildProcessManager;
use harnx_toolset::server_identity_token;
use harnx_toolset_server::{registration_key, TOOL_REGISTRY_BUCKET};
use std::{path::PathBuf, process::Stdio};

pub(in super::super) struct BridgeScript {
    pub server_name: &'static str,
    pub tool_name: &'static str,
    pub result: Value,
}

pub(in super::super) struct McpBridge {
    child: tokio::process::Child,
    audit: PathBuf,
    _children: ChildProcessManager,
}

impl McpBridge {
    pub async fn start(env: &ResultEnvironment, script: BridgeScript) -> Result<Self> {
        let path = env.root.path().join("mcp.json");
        let audit = env.root.path().join("requests.jsonl");
        std::fs::write(
            &path,
            serde_json::to_vec(&json!({
                "tools": [{"name": script.tool_name}], "responses": ["unused"],
                "call_result": script.result
            }))?,
        )?;
        let children = ChildProcessManager::new();
        let mut command = tokio::process::Command::new(binary("harnx-mcp-bridge")?);
        command
            .args(["--name", script.server_name, "--max-result-bytes"])
            .arg(BRIDGE_CAP.to_string())
            .arg("--")
            .arg(binary("harnx-mock-mcp")?)
            .arg("--script")
            .arg(path)
            .arg("--request-log")
            .arg(&audit)
            .env("HARNX_SERVER_SCOPE", env.scope.as_str())
            .env("HARNX_NATS_URL", env.broker.url())
            .env_remove("HARNX_NATS_TOKEN")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .kill_on_drop(true);
        let mut bridge = Self {
            child: children.spawn(command).await?,
            audit,
            _children: children,
        };
        bridge.wait_for_servers(env, &[script.server_name]).await?;
        Ok(bridge)
    }

    pub async fn wait_for_servers(
        &mut self,
        env: &ResultEnvironment,
        names: &[&str],
    ) -> Result<()> {
        poll_until(async || {
            if let Some(status) = self.child.try_wait()? {
                anyhow::bail!("bridge exited: {status}");
            }
            let Ok(registry) = env.js.get_key_value(TOOL_REGISTRY_BUCKET).await else {
                return Ok(false);
            };
            for name in names {
                let identity = server_identity_token(None, "", name);
                if registry
                    .get(registration_key(&env.scope, &identity))
                    .await?
                    .is_none()
                {
                    return Ok(false);
                }
            }
            Ok(true)
        })
        .await
    }

    pub fn call_count(&self) -> Result<usize> {
        let entries = std::fs::read_to_string(&self.audit)?
            .lines()
            .map(serde_json::from_str::<Value>)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(entries
            .iter()
            .filter(|entry| entry["method"] == "tools/call")
            .count())
    }

    pub async fn stop(mut self) -> Result<()> {
        #[cfg(unix)]
        {
            let pid = self.child.id().context("bridge pid")?;
            anyhow::ensure!(
                unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) } == 0,
                "signal bridge"
            );
        }
        #[cfg(not(unix))]
        self.child.kill().await?;
        tokio::time::timeout(CI_SAFE_TIMEOUT, self.child.wait()).await??;
        Ok(())
    }
}

fn binary(name: &str) -> Result<PathBuf> {
    let mut path = std::env::current_exe()?;
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.push(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    anyhow::ensure!(
        path.is_file(),
        "build {name} before result integration tests"
    );
    Ok(path)
}

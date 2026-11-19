//! Hook execution for harnx-sandbox-run using MCP stdio environment tools.
//!
//! Converts CLI hook definitions into MCP stdio sidecar processes, requests
//! their `environment` tool, and extracts environment variable mutations.
//!
//! `ServiceManager` retains the child processes (and their stdio streams) alive
//! so that sidecar services (e.g. `harnx-proxy-auth`, `harnx-aws-creds`, `harnx-k8s-creds`)
//! stay running for the full duration of the sandboxed command.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use crate::environment::parse_environment_tool_result;
use anyhow::{bail, Context, Result};
use rmcp::handler::client::ClientHandler;
use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, Implementation, InitializeRequestParams,
};
use rmcp::service::{serve_client, RoleClient};
use rmcp::transport::async_rw::AsyncRwTransport;
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

use crate::cli::HookDef;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const CALL_TOOL_TIMEOUT: Duration = Duration::from_secs(10);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(500);
const MAX_MCP_MESSAGE_BYTES: usize = 256 * 1024;

#[derive(Clone, Default)]
struct SandboxClientHandler;

impl ClientHandler for SandboxClientHandler {
    fn get_info(&self) -> InitializeRequestParams {
        InitializeRequestParams::new(
            ClientCapabilities::default(),
            Implementation::new("harnx-sandbox-run", env!("CARGO_PKG_VERSION")),
        )
    }
}

/// Active service session kept alive while the sandboxed process runs.
pub struct ServiceSession {
    child: Child,
    client_task: JoinHandle<()>,
}

async fn cleanup_child(mut child: Child, client_task: Option<JoinHandle<()>>) {
    if let Some(task) = client_task {
        task.abort();
    }
    match tokio::time::timeout(SHUTDOWN_TIMEOUT, child.wait()).await {
        Ok(_) => {}
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }
}

impl ServiceSession {
    pub async fn shutdown(self) {
        cleanup_child(self.child, Some(self.client_task)).await;
    }
}

/// Manager holding all active sidecar services.
///
/// Dropping this manager kills all spawned sidecar children.
pub struct ServiceManager {
    sessions: Vec<ServiceSession>,
}

impl Default for ServiceManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ServiceManager {
    pub fn new() -> Self {
        Self {
            sessions: Vec::new(),
        }
    }

    pub fn add(&mut self, session: ServiceSession) {
        self.sessions.push(session);
    }

    pub async fn shutdown(mut self) {
        for session in self.sessions.drain(..) {
            session.shutdown().await;
        }
    }
}

impl Drop for ServiceManager {
    fn drop(&mut self) {
        for session in &mut self.sessions {
            session.client_task.abort();
            let _ = session.child.start_kill();
        }
    }
}

/// Result of hook dispatch: environment mutations partitioned by precedence plus the live service manager.
pub struct HookResult {
    /// Proxy environment additions (caller-wins).
    pub proxy_env: HashMap<String, String>,
    /// Credential environment additions (provider-wins).
    pub credential_env: HashMap<String, String>,
    /// Live service manager — kept alive until the sandboxed child exits.
    pub manager: ServiceManager,
}

/// Run configured hooks via MCP stdio, collect env mutations, and return HookResult.
pub async fn run_hooks(hook_defs: &[HookDef], cwd: &Path) -> Result<HookResult> {
    let mut manager = ServiceManager::new();
    let mut proxy_env = HashMap::new();
    let mut credential_env = HashMap::new();

    for def in hook_defs {
        let (env, is_proxy, session) = match execute_mcp_hook(def, cwd).await {
            Ok(res) => res,
            Err(err) => {
                manager.shutdown().await;
                return Err(err);
            }
        };
        if is_proxy {
            proxy_env.extend(env);
        } else {
            credential_env.extend(env);
        }
        manager.add(session);
    }

    Ok(HookResult {
        proxy_env,
        credential_env,
        manager,
    })
}

fn spawn_hook_child(def: &HookDef, cwd: &Path) -> Result<Child> {
    let mut cmd = Command::new(&def.command);
    cmd.args(&def.args);
    if !def
        .args
        .iter()
        .any(|a| a == "--mcp-stdio" || a.starts_with("--mcp-stdio="))
    {
        cmd.arg("--mcp-stdio");
    }
    cmd.current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| {
            format!(
                "failed to spawn hook command '{}' (args: {:?})",
                def.command, def.args
            )
        })
}

async fn handshake_mcp_client(
    child: &mut Child,
    command: &str,
) -> Result<(rmcp::service::Peer<RoleClient>, JoinHandle<()>)> {
    let stdin = child
        .stdin
        .take()
        .context("failed to open stdin for hook child")?;
    let stdout = child
        .stdout
        .take()
        .context("failed to open stdout for hook child")?;

    let transport = AsyncRwTransport::<RoleClient, _, _>::new(stdout, stdin)
        .with_max_line_length(MAX_MCP_MESSAGE_BYTES);
    let client_service = match tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        serve_client(SandboxClientHandler, transport),
    )
    .await
    {
        Err(_) => bail!(
            "MCP initialize timed out ({:?}) for hook command '{}'",
            HANDSHAKE_TIMEOUT,
            command
        ),
        Ok(res) => res.with_context(|| format!("MCP handshake failed for '{command}'"))?,
    };

    let peer = client_service.peer().clone();
    let client_task = tokio::spawn(async move {
        let _ = client_service.waiting().await;
    });

    Ok((peer, client_task))
}

async fn request_environment(
    peer: &rmcp::service::Peer<RoleClient>,
    command: &str,
) -> Result<HashMap<String, String>> {
    let tool_params =
        CallToolRequestParams::new("environment").with_arguments(serde_json::Map::new());
    let call_result =
        match tokio::time::timeout(CALL_TOOL_TIMEOUT, peer.call_tool(tool_params)).await {
            Err(_) => bail!(
                "MCP call_tool 'environment' timed out ({:?}) for hook command '{}'",
                CALL_TOOL_TIMEOUT,
                command
            ),
            Ok(res) => {
                res.with_context(|| format!("MCP call_tool 'environment' failed for '{command}'"))?
            }
        };

    if call_result.is_error == Some(true) {
        bail!("MCP tool 'environment' failed with isError=true for hook command '{command}'");
    }

    parse_environment_tool_result(&call_result.content)
}

async fn execute_mcp_hook(
    def: &HookDef,
    cwd: &Path,
) -> Result<(HashMap<String, String>, bool, ServiceSession)> {
    if def.hook_type != "environment" {
        bail!(
            "hook type '{}' is not supported in harnx-sandbox-run. Migrate to '--hook environment <COMMAND> [ARGS...] ;' using MCP stdio",
            def.hook_type
        );
    }

    let mut child = spawn_hook_child(def, cwd)?;
    let (peer, client_task) = match handshake_mcp_client(&mut child, &def.command).await {
        Ok(res) => res,
        Err(err) => {
            cleanup_child(child, None).await;
            return Err(err);
        }
    };
    let env = match request_environment(&peer, &def.command).await {
        Ok(env) => env,
        Err(err) => {
            cleanup_child(child, Some(client_task)).await;
            return Err(err);
        }
    };

    let is_proxy = def.command.contains("proxy")
        || env.contains_key("HTTP_PROXY")
        || env.contains_key("HTTPS_PROXY");

    Ok((env, is_proxy, ServiceSession { child, client_task }))
}

#[cfg(test)]
#[cfg(unix)]
mod tests;

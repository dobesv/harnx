// rmcp deprecated MCP Roots (SEP-2577); bridge still returns method_not_found.
#![allow(deprecated)]

use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use async_trait::async_trait;
use clap::Parser;
use harnx_toolset::{ToolInvokeError, ToolSpec, Toolset};
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
use process_wrap::tokio::{ChildWrapper, CommandWrap};
use rmcp::handler::client::ClientHandler;
use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ErrorData, Implementation, InitializeRequestParams,
    ListRootsResult, Tool,
};
use rmcp::service::{Peer, RequestContext, RoleClient, ServiceError};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout, Command};
use tokio_util::sync::CancellationToken;

const STDERR_TAIL_LINES: usize = 50;
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(30);
const LIST_TOOLS_TIMEOUT: Duration = Duration::from_secs(10);

type StderrTail = Arc<Mutex<VecDeque<String>>>;
type SpawnedChild = (
    Box<dyn ChildWrapper>,
    ChildStdin,
    ChildStdout,
    Option<ChildStderr>,
);

/// Command-line arguments for the MCP-to-NATS bridge.
#[derive(Debug, Clone, PartialEq, Eq, Parser)]
#[command(trailing_var_arg = true)]
pub struct Args {
    /// Server name used for registration and tool-name prefixes.
    #[arg(long)]
    pub name: String,

    /// Wrapped MCP command followed by its arguments.
    #[arg(required = true, allow_hyphen_values = true)]
    pub child: Vec<String>,
}

impl Args {
    /// Parses bridge arguments from the process command line.
    pub fn parse() -> Self {
        <Self as Parser>::parse()
    }

    /// Parses bridge arguments from an argument iterator.
    pub fn parse_from<I, T>(iter: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        <Self as Parser>::parse_from(iter)
    }
}

/// Connected stdio MCP server and its tool metadata cached at startup.
pub struct BridgeToolset {
    server_name: String,
    cached_tools: Vec<ToolSpec>,
    peer: Peer<RoleClient>,
    child_died: CancellationToken,
    _service_watch: tokio::task::JoinHandle<()>,
    _child: Box<dyn ChildWrapper>,
    stderr_tail: StderrTail,
}

fn spawn_child(server_name: &str, program: &str, args: &[String]) -> anyhow::Result<SpawnedChild> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    configure_child_process(&mut command);

    // A separate process group prevents terminal SIGINT from reaching the child.
    #[allow(unused_mut)]
    let mut wrap = CommandWrap::from(command);
    #[cfg(unix)]
    wrap.wrap(ProcessGroup::leader());

    let mut child = wrap
        .spawn()
        .with_context(|| format!("Failed to spawn MCP server '{server_name}'"))?;
    let stdin = child
        .stdin()
        .take()
        .ok_or_else(|| anyhow!("MCP server '{server_name}' stdin not piped"))?;
    let stdout = child
        .stdout()
        .take()
        .ok_or_else(|| anyhow!("MCP server '{server_name}' stdout not piped"))?;
    let stderr = child.stderr().take();
    Ok((child, stdin, stdout, stderr))
}

fn spawn_stderr_reader(server_name: &str, stderr: Option<ChildStderr>) -> StderrTail {
    let stderr_tail = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL_LINES)));
    if let Some(stderr) = stderr {
        let reader_server_name = server_name.to_owned();
        let reader_tail = Arc::clone(&stderr_tail);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                log::debug!("[mcp:{reader_server_name}] {line}");
                let mut tail = reader_tail.lock().unwrap_or_else(|err| err.into_inner());
                if tail.len() == STDERR_TAIL_LINES {
                    tail.pop_front();
                }
                tail.push_back(line);
            }
        });
    }
    stderr_tail
}

async fn connect_and_list_tools(
    server_name: &str,
    stdin: ChildStdin,
    stdout: ChildStdout,
    stderr_tail: &StderrTail,
) -> anyhow::Result<(
    rmcp::service::RunningService<RoleClient, BridgeClientHandler>,
    Vec<ToolSpec>,
)> {
    let transport =
        rmcp::transport::async_rw::AsyncRwTransport::<RoleClient, _, _>::new(stdout, stdin);
    let service = match tokio::time::timeout(
        INITIALIZE_TIMEOUT,
        rmcp::service::serve_client(BridgeClientHandler, transport),
    )
    .await
    {
        Err(_) => bail!(
            "MCP server '{}' timed out during initialization (30s){}",
            server_name,
            render_stderr_tail(stderr_tail)
        ),
        Ok(Err(err)) => {
            return Err(anyhow::Error::from(err)).with_context(|| {
                format!(
                    "Failed to initialize MCP client for server '{}'{}",
                    server_name,
                    render_stderr_tail(stderr_tail)
                )
            });
        }
        Ok(Ok(service)) => service,
    };

    let listed = match tokio::time::timeout(
        LIST_TOOLS_TIMEOUT,
        service.peer().list_tools(Default::default()),
    )
    .await
    {
        Err(_) => bail!(
            "MCP server '{}' timed out listing tools (10s){}",
            server_name,
            render_stderr_tail(stderr_tail)
        ),
        Ok(Err(err)) => {
            return Err(anyhow::Error::from(err)).with_context(|| {
                format!(
                    "Failed to list tools for MCP server '{}'{}",
                    server_name,
                    render_stderr_tail(stderr_tail)
                )
            });
        }
        Ok(Ok(result)) => result,
    };
    let cached_tools = listed
        .tools
        .into_iter()
        .map(|tool| map_tool(server_name, tool))
        .collect();

    Ok((service, cached_tools))
}

impl BridgeToolset {
    /// Spawns a stdio MCP server, initializes its client connection, and lists tools once.
    pub async fn new(
        server_name: impl Into<String>,
        child_argv: Vec<String>,
    ) -> anyhow::Result<Self> {
        let server_name = server_name.into();
        let (program, args) = child_argv
            .split_first()
            .ok_or_else(|| anyhow!("MCP server '{}' command is empty", server_name))?;
        let (child, stdin, stdout, stderr) = spawn_child(&server_name, program, args)?;
        let stderr_tail = spawn_stderr_reader(&server_name, stderr);
        let (service, cached_tools) =
            connect_and_list_tools(&server_name, stdin, stdout, &stderr_tail).await?;

        let peer = service.peer().clone();
        let child_died = CancellationToken::new();
        let watch_token = child_died.clone();
        let watch_name = server_name.clone();
        let watch_stderr = Arc::clone(&stderr_tail);
        // RunningService owns the transport loop, so its completion reports child stdio closure
        // without moving the process handle needed for kill_on_drop out of BridgeToolset.
        let service_watch = tokio::spawn(async move {
            let reason = service.waiting().await;
            log::warn!(
                "MCP server '{watch_name}' connection closed ({reason:?}){}",
                render_stderr_tail(&watch_stderr)
            );
            watch_token.cancel();
        });

        Ok(Self {
            server_name,
            cached_tools,
            peer,
            child_died,
            _service_watch: service_watch,
            _child: child,
            stderr_tail,
        })
    }

    /// Server name used to prefix cached tool names.
    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// Tool metadata returned by the child's initial tools/list response.
    pub fn cached_tools(&self) -> &[ToolSpec] {
        &self.cached_tools
    }

    /// Client peer connected to the wrapped MCP server.
    pub fn peer(&self) -> &Peer<RoleClient> {
        &self.peer
    }

    /// Current bounded tail of child stderr, oldest line first.
    pub fn stderr_tail(&self) -> Vec<String> {
        self.stderr_tail
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    /// Process ID of the wrapped MCP server, when available.
    pub fn child_id(&self) -> Option<u32> {
        self._child.id()
    }

    /// Token cancelled when the wrapped server's MCP transport closes.
    pub fn child_died_token(&self) -> CancellationToken {
        self.child_died.clone()
    }
}

#[async_trait]
impl Toolset for BridgeToolset {
    fn name(&self) -> &str {
        &self.server_name
    }

    fn tools(&self) -> Vec<ToolSpec> {
        self.cached_tools.clone()
    }

    async fn invoke(
        &self,
        tool: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<Value, ToolInvokeError> {
        let prefix = format!("{}_", self.server_name);
        let mcp_tool = tool
            .strip_prefix(&prefix)
            .ok_or_else(|| ToolInvokeError::Recoverable(format!("bad tool name {tool}")))?;
        let arguments = match args {
            Value::Object(arguments) => Some(arguments),
            Value::Null => None,
            _ => {
                return Err(ToolInvokeError::Recoverable(
                    "MCP tool arguments must be a JSON object or null".into(),
                ));
            }
        };
        let mut params = CallToolRequestParams::new(mcp_tool.to_owned());
        if let Some(arguments) = arguments {
            params = params.with_arguments(arguments);
        }

        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                Err(ToolInvokeError::Recoverable("call cancelled".into()))
            }
            result = self.peer.call_tool(params) => match result {
                Ok(result) => serde_json::to_value(result)
                    .map_err(|err| ToolInvokeError::Fatal(err.to_string())),
                Err(ServiceError::TransportClosed | ServiceError::TransportSend(_)) => {
                    Err(ToolInvokeError::Fatal(format!(
                        "MCP server '{}' exited during call",
                        self.server_name
                    )))
                }
                Err(err) => Err(ToolInvokeError::Recoverable(err.to_string())),
            },
        }
    }
}

#[cfg(target_os = "linux")]
fn configure_child_process(command: &mut Command) {
    let parent_pid = std::process::id() as libc::pid_t;
    // SAFETY: pre_exec invokes only async-signal-safe libc calls. ProcessGroup configures setpgid.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != parent_pid {
                libc::raise(libc::SIGTERM);
            }
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn configure_child_process(_command: &mut Command) {}

fn map_tool(server_name: &str, tool: Tool) -> ToolSpec {
    let annotations = tool.annotations.as_ref();
    ToolSpec {
        name: format!("{server_name}_{}", tool.name),
        description: tool
            .description
            .map_or_else(String::new, |value| value.into_owned()),
        input_schema: Value::Object((*tool.input_schema).clone()),
        idempotent_hint: annotations
            .and_then(|value| value.idempotent_hint)
            .unwrap_or(false),
        read_only_hint: annotations
            .and_then(|value| value.read_only_hint)
            .unwrap_or(false),
        timeout_secs: None,
    }
}

fn render_stderr_tail(stderr_tail: &StderrTail) -> String {
    let tail = stderr_tail.lock().unwrap_or_else(|err| err.into_inner());
    if tail.is_empty() {
        String::new()
    } else {
        format!(
            "\nChild stderr tail:\n{}",
            tail.iter().cloned().collect::<Vec<_>>().join("\n")
        )
    }
}

/// Client handler for wrapped servers. S1 doesn't expose roots to children.
pub struct BridgeClientHandler;

impl ClientHandler for BridgeClientHandler {
    fn get_info(&self) -> InitializeRequestParams {
        InitializeRequestParams::new(
            ClientCapabilities::default(),
            Implementation::new("harnx-mcp-bridge", env!("CARGO_PKG_VERSION")),
        )
    }

    async fn list_roots(
        &self,
        _context: RequestContext<RoleClient>,
    ) -> Result<ListRootsResult, ErrorData> {
        Err(ErrorData::method_not_found::<
            rmcp::model::ListRootsRequestMethod,
        >())
    }
}

#[cfg(test)]
mod tests {
    use super::Args;
    #[cfg(unix)]
    use super::BridgeToolset;

    #[test]
    fn arg_parses_child_command_and_flags_after_separator() {
        let args = Args::parse_from([
            "harnx-mcp-bridge",
            "--name",
            "plans",
            "--",
            "harnx-plans-tools",
            "--mcp-stdio",
            "--dir",
            ".agent/plans",
        ]);

        assert_eq!(args.name, "plans");
        assert_eq!(
            args.child,
            ["harnx-plans-tools", "--mcp-stdio", "--dir", ".agent/plans"]
        );
    }

    #[test]
    fn arg_parses_child_command_without_child_arguments() {
        let args = Args::parse_from([
            "harnx-mcp-bridge",
            "--name",
            "plans",
            "--",
            "harnx-plans-tools",
        ]);

        assert_eq!(args.name, "plans");
        assert_eq!(args.child, ["harnx-plans-tools"]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn caches_prefixed_plans_tool_specs() {
        let plans_dir = tempfile::tempdir().expect("create temporary plans directory");
        let plans_binary = option_env!("CARGO_BIN_EXE_harnx-plans-tools")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                let mut path = std::env::current_exe().expect("locate current test executable");
                path.pop();
                if path.ends_with("deps") {
                    path.pop();
                }
                path.join("harnx-plans-tools")
            });
        assert!(
            plans_binary.is_file(),
            "harnx-plans-tools binary missing at {}; build it before this test",
            plans_binary.display()
        );
        let bridge = BridgeToolset::new(
            "plans",
            vec![
                plans_binary.display().to_string(),
                "--mcp-stdio".to_owned(),
                "--dir".to_owned(),
                plans_dir.path().display().to_string(),
            ],
        )
        .await
        .expect("connect to plans MCP server");

        assert_eq!(bridge.cached_tools().len(), 15);
        assert!(bridge.cached_tools().iter().all(|tool| {
            tool.name.starts_with("plans_")
                && tool
                    .input_schema
                    .as_object()
                    .is_some_and(|schema| !schema.is_empty())
        }));
    }

    #[cfg(unix)]
    fn plans_command(dir: &std::path::Path) -> Vec<String> {
        let plans_binary = option_env!("CARGO_BIN_EXE_harnx-plans-tools")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                let mut path = std::env::current_exe().expect("locate current test executable");
                path.pop();
                if path.ends_with("deps") {
                    path.pop();
                }
                path.join("harnx-plans-tools")
            });
        assert!(
            plans_binary.is_file(),
            "harnx-plans-tools binary missing at {}; build it before this test",
            plans_binary.display()
        );
        vec![
            plans_binary.display().to_string(),
            "--mcp-stdio".to_owned(),
            "--dir".to_owned(),
            dir.display().to_string(),
        ]
    }

    #[cfg(unix)]
    fn process_exists(pid: u32) -> bool {
        // SAFETY: signal 0 only checks whether the process exists.
        let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_bridge_terminates_child() {
        let plans_dir = tempfile::tempdir().expect("create temporary plans directory");
        let bridge = BridgeToolset::new("plans", plans_command(plans_dir.path()))
            .await
            .expect("connect to plans MCP server");
        let pid = bridge.child_id().expect("child process ID");

        drop(bridge);

        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while process_exists(pid) {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("child should exit after bridge drop");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn child_death_cancels_token() {
        let plans_dir = tempfile::tempdir().expect("create temporary plans directory");
        let bridge = BridgeToolset::new("plans", plans_command(plans_dir.path()))
            .await
            .expect("connect to plans MCP server");
        let pid = bridge.child_id().expect("child process ID");
        let child_died = bridge.child_died_token();

        // SAFETY: pid belongs to the child held by bridge.
        assert_eq!(unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) }, 0);
        tokio::time::timeout(std::time::Duration::from_secs(5), child_died.cancelled())
            .await
            .expect("child death token should fire");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn invokes_child_tools_and_validates_name_and_cancellation() {
        use harnx_toolset::{ToolInvokeError, Toolset};
        use tokio_util::sync::CancellationToken;

        let plans_dir = tempfile::tempdir().expect("create temporary plans directory");
        let bridge = BridgeToolset::new("plans", plans_command(plans_dir.path()))
            .await
            .expect("connect to plans MCP server");

        let result = bridge
            .invoke(
                "plans_list_plans",
                serde_json::json!({}),
                CancellationToken::new(),
            )
            .await
            .expect("invoke plans list_plans");
        assert!(result
            .get("content")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|content| !content.is_empty()));

        let bad_name = bridge
            .invoke(
                "wrongname_foo",
                serde_json::json!({}),
                CancellationToken::new(),
            )
            .await;
        assert!(matches!(
            bad_name,
            Err(ToolInvokeError::Recoverable(message)) if message == "bad tool name wrongname_foo"
        ));

        let bad_arguments = bridge
            .invoke(
                "plans_list_plans",
                serde_json::json!([]),
                CancellationToken::new(),
            )
            .await;
        assert!(matches!(
            bad_arguments,
            Err(ToolInvokeError::Recoverable(message))
                if message == "MCP tool arguments must be a JSON object or null"
        ));

        let cancel = CancellationToken::new();
        cancel.cancel();
        let cancelled = bridge
            .invoke("plans_list_plans", serde_json::json!({}), cancel)
            .await;
        assert!(matches!(
            cancelled,
            Err(ToolInvokeError::Recoverable(message)) if message == "call cancelled"
        ));
    }
}

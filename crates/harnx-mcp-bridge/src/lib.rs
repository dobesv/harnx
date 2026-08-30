use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use async_trait::async_trait;
use clap::Parser;
use harnx_core::instance::HARNX_SERVER_SCOPE;
use harnx_toolset::{ToolInvokeError, ToolSpec, Toolset};
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
use process_wrap::tokio::{ChildWrapper, CommandWrap};
use rmcp::handler::client::ClientHandler;
use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, Implementation, InitializeRequestParams, Tool,
};
use rmcp::service::{Peer, RoleClient, ServiceError};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout, Command};
use tokio_util::sync::CancellationToken;

const STDERR_TAIL_LINES: usize = 50;
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(30);
/// Spacing of the "still waiting" notices during a slow handshake.
const HANDSHAKE_PROGRESS_INTERVAL: Duration = Duration::from_secs(5);
const LIST_TOOLS_TIMEOUT: Duration = Duration::from_secs(10);

type StderrTail = Arc<Mutex<VecDeque<String>>>;
type SpawnedChild = (
    Box<dyn ChildWrapper>,
    ChildStdin,
    ChildStdout,
    Option<ChildStderr>,
);

/// Command-line arguments for the MCP-to-NATS bridge.
#[derive(Clone, Debug, PartialEq, Eq, Parser)]
#[command(trailing_var_arg = true)]
pub struct Args {
    /// Server name used for registration. Required when serving over NATS;
    /// optional for `--list-tools`, which does not register.
    #[arg(long)]
    pub name: Option<String>,

    /// Start the wrapped server, print the tools it advertises, and exit
    /// without connecting to NATS.
    ///
    /// Diagnostic for a server that never registers: it separates "the child
    /// does not start", "it starts but never completes the MCP handshake", and
    /// "it works and the problem is elsewhere", which are indistinguishable
    /// from the supervisor's registration timeout alone.
    #[arg(long)]
    pub list_tools: bool,

    /// Wrapped MCP command followed by its arguments.
    #[arg(required = true, allow_hyphen_values = true)]
    pub child: Vec<String>,

    /// Prometheus metrics endpoint configuration.
    #[command(flatten)]
    pub metrics: harnx_metrics::MetricsFlags,
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

/// Render what a wrapped server advertises, for `--list-tools`.
///
/// Reaching this at all establishes that the child starts, completes the MCP
/// handshake, and answers `tools/list` — the three steps a registration
/// timeout cannot distinguish between.
pub fn report_tools(bridge: &BridgeToolset) -> String {
    let tools = bridge.cached_tools();
    let mut out = format!(
        "MCP server '{}': {} tool(s)\n",
        bridge.server_name(),
        tools.len()
    );
    for tool in tools {
        out.push_str(&format!("\n  {}\n", tool.name));
        let description = tool.description.trim();
        if !description.is_empty() {
            out.push_str(&format!(
                "    {}\n",
                description.lines().next().unwrap_or("")
            ));
        }
        let hints = [
            tool.read_only_hint.then_some("read-only"),
            tool.idempotent_hint.then_some("idempotent"),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        if !hints.is_empty() {
            out.push_str(&format!("    [{}]\n", hints.join(", ")));
        }
        if let Some(seconds) = tool.timeout_secs {
            out.push_str(&format!("    timeout: {seconds}s\n"));
        }
    }
    if tools.is_empty() {
        out.push_str("\n  (the server completed its handshake but advertises no tools)\n");
    }
    out
}

/// Summarise the inherited settings that decide whether a wrapped server can
/// reach the network and find its runtime.
///
/// A server that starts in a shell but stalls under the worker differs only by
/// the environment it inherits, and a proxy pointing at a port that is gone
/// makes package managers block silently. Names only for anything that could
/// carry a credential.
fn child_runtime_env() -> String {
    const SHOWN: [&str; 6] = [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "NO_PROXY",
        "NODE_EXTRA_CA_CERTS",
        "npm_config_registry",
        "npm_config_proxy",
    ];
    let mut parts: Vec<String> = SHOWN
        .iter()
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .filter(|value| !value.is_empty())
                .map(|value| format!("{name}={value}"))
        })
        .collect();
    let secretish = ["NPM_TOKEN", "npm_config__auth", "EXA_API_KEY"]
        .iter()
        .filter(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
        .copied()
        .collect::<Vec<_>>();
    if !secretish.is_empty() {
        parts.push(format!("set(no values shown): {}", secretish.join(", ")));
    }
    if parts.is_empty() {
        return "no proxy or registry overrides inherited".to_string();
    }
    format!("inherited {}", parts.join(" "))
}

/// Report that a handshake is still outstanding, every few seconds.
///
/// Without this a slow start and a wedged one look identical until the 30s
/// timeout, by which point a short-lived front-end has often already exited and
/// taken the evidence with it.
fn spawn_handshake_progress(server_name: &str) -> tokio::task::JoinHandle<()> {
    let server_name = server_name.to_owned();
    tokio::spawn(async move {
        let started = std::time::Instant::now();
        loop {
            tokio::time::sleep(HANDSHAKE_PROGRESS_INTERVAL).await;
            log::warn!(
                "MCP server '{server_name}': still waiting for the MCP handshake after {:.0}s",
                started.elapsed().as_secs_f64()
            );
        }
    })
}

/// The worker identity a bridge must not pass on.
///
/// The bridge is the process that registers over NATS; everything below it
/// speaks MCP on stdio. Leaking these lets a descendant conclude it was
/// launched by a worker and switch to a NATS protocol — `harnx-proxy-auth`,
/// run as a stdio hook by a sandbox shim, does exactly that and then never
/// answers the stdio handshake, so the wrapped server is never launched at all.
const WORKER_NATS_ENV: [&str; 3] = [HARNX_SERVER_SCOPE, "HARNX_NATS_URL", "HARNX_NATS_TOKEN"];

fn spawn_child(server_name: &str, program: &str, args: &[String]) -> anyhow::Result<SpawnedChild> {
    let mut command = Command::new(program);
    for name in WORKER_NATS_ENV {
        command.env_remove(name);
    }
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    harnx_telemetry::forward_otel_env_without_credentials(&mut command);
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
    let cached_tools = listed.tools.into_iter().map(map_tool).collect();

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
        log::info!(
            "MCP server '{server_name}': starting {}",
            shell_words::join(&child_argv)
        );
        log::info!("MCP server '{server_name}': {}", child_runtime_env());
        // Resolve argv[0] the way the OS will. A wrapper shim earlier on PATH
        // than the real binary is invisible in the command line but changes what
        // actually runs, and a shim that prints anything of its own corrupts a
        // stdio protocol before the server it wraps ever speaks.
        log::info!(
            "MCP server '{server_name}': '{program}' resolves to {}",
            which::which(program)
                .map(|path| path.display().to_string())
                .unwrap_or_else(|error| format!("<unresolved: {error}>"))
        );
        let (child, stdin, stdout, stderr) = spawn_child(&server_name, program, args)?;
        // The pid makes a stalled handshake inspectable from outside: the child
        // is a launcher like `npx`, so knowing which process to look at is the
        // difference between "it is slow" and a `/proc` answer for why.
        log::info!(
            "MCP server '{server_name}': child pid {}",
            child
                .id()
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "unknown".to_string())
        );
        let stderr_tail = spawn_stderr_reader(&server_name, stderr);
        let handshake = std::time::Instant::now();
        let progress = spawn_handshake_progress(&server_name);
        let (service, cached_tools) =
            connect_and_list_tools(&server_name, stdin, stdout, &stderr_tail).await?;
        progress.abort();
        log::info!(
            "MCP server '{server_name}': handshake completed in {:.1}s",
            handshake.elapsed().as_secs_f64()
        );

        log::info!(
            "MCP server '{server_name}': ready with {} tool(s)",
            cached_tools.len()
        );
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

    /// Server name used for toolset registration.
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

fn prepare_call_tool_params(
    tool: &str,
    arguments: Option<serde_json::Map<String, Value>>,
) -> CallToolRequestParams {
    let mut params = CallToolRequestParams::new(tool.to_owned());
    if let Some(arguments) = arguments {
        params = params.with_arguments(arguments);
    }
    harnx_telemetry::propagate::inject_current_into_mcp(&mut params);
    params
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
        let arguments = match args {
            Value::Object(arguments) => Some(arguments),
            Value::Null => None,
            _ => {
                return Err(ToolInvokeError::Recoverable(
                    "MCP tool arguments must be a JSON object or null".into(),
                ));
            }
        };
        let params = prepare_call_tool_params(tool, arguments);

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

fn map_tool(tool: Tool) -> ToolSpec {
    let annotations = tool.annotations.as_ref();
    ToolSpec {
        name: tool.name.into(),
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
        meta: tool.meta.map(|m| m.0),
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

/// Client handler for wrapped servers.
pub struct BridgeClientHandler;

impl ClientHandler for BridgeClientHandler {
    fn get_info(&self) -> InitializeRequestParams {
        InitializeRequestParams::new(
            ClientCapabilities::default(),
            Implementation::new("harnx-mcp-bridge", env!("CARGO_PKG_VERSION")),
        )
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::BridgeToolset;
    // Only the unix tests exercise a real child process, so the reporter is
    // unused elsewhere and `-D warnings` rejects the import.
    #[cfg(unix)]
    use super::report_tools;
    use super::{map_tool, prepare_call_tool_params, Args};
    use opentelemetry::global;
    use opentelemetry::trace::{TraceContextExt, TracerProvider as _};
    use opentelemetry_sdk::propagation::TraceContextPropagator;
    use opentelemetry_sdk::trace::SdkTracerProvider;
    use rmcp::model::RequestParamsMeta;
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    use tracing_subscriber::layer::SubscriberExt;

    #[test]
    fn call_tool_params_carry_active_trace_id() {
        harnx_core::require_nextest();
        global::set_text_map_propagator(TraceContextPropagator::new());
        let provider = SdkTracerProvider::builder().build();
        let tracer = provider.tracer("mcp-bridge-propagation-test");
        let subscriber =
            tracing_subscriber::registry().with(tracing_opentelemetry::layer().with_tracer(tracer));

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("mcp_bridge_propagation_test");
            let _entered = span.enter();
            let expected_trace_id = tracing::Span::current()
                .context()
                .span()
                .span_context()
                .trace_id();
            assert_ne!(expected_trace_id, opentelemetry::trace::TraceId::INVALID);

            let params = prepare_call_tool_params(
                "test_tool",
                serde_json::json!({ "value": 1 }).as_object().cloned(),
            );
            let traceparent = params.traceparent().expect("traceparent in MCP _meta");
            assert!(
                traceparent.starts_with(&format!("00-{expected_trace_id}-")),
                "traceparent {traceparent} did not carry trace ID {expected_trace_id}"
            );
        });
    }

    #[test]
    fn list_tools_parses_without_a_name_but_serving_still_needs_one() {
        let listing = Args::parse_from(["bridge", "--list-tools", "--", "npx", "-y", "srv"]);
        assert!(listing.list_tools);
        assert_eq!(listing.name, None);
        assert_eq!(listing.child, vec!["npx", "-y", "srv"]);

        let serving = Args::parse_from(["bridge", "--name", "exa", "--", "npx", "-y", "srv"]);
        assert!(!serving.list_tools);
        assert_eq!(serving.name.as_deref(), Some("exa"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn list_tools_reports_the_wrapped_server_inventory() {
        let Some(binary) = plans_tools_binary() else {
            eprintln!("skipping: harnx-plans-tools not built");
            return;
        };
        let bridge = BridgeToolset::new(
            "diag",
            vec![binary.display().to_string(), "--mcp-stdio".to_string()],
        )
        .await
        .expect("wrap plans tools");

        let report = report_tools(&bridge);
        assert!(report.starts_with("MCP server 'diag':"), "{report}");
        assert!(report.contains("list_plans"), "{report}");
        // Hints come from the child's advertised metadata, not from defaults.
        assert!(report.contains("[read-only]"), "{report}");
    }

    /// A sandbox shim in the child chain runs `harnx-proxy-auth` as a stdio
    /// hook, and that binary switches to a NATS protocol when it finds a
    /// worker's identity in the environment — leaving the shim waiting on a
    /// stdio reply that never comes, so the wrapped server never starts.
    #[cfg(unix)]
    #[tokio::test]
    async fn wrapped_child_does_not_inherit_the_workers_nats_identity() {
        harnx_core::require_nextest();
        let Some(binary) = plans_tools_binary() else {
            eprintln!("skipping: harnx-plans-tools not built");
            return;
        };
        // SAFETY: nextest gives this test its own process.
        unsafe {
            std::env::set_var("HARNX_NATS_URL", "nats://127.0.0.1:1");
            std::env::set_var("HARNX_NATS_TOKEN", "unused");
            std::env::set_var(super::HARNX_SERVER_SCOPE, "worker-instance");
        }

        // Stands in for the shim: refuse to exec the real server if any of the
        // worker's identity survived into the child.
        let guard = format!(
            r#"if [ -n "${{HARNX_NATS_URL:-}}" ] || [ -n "${{HARNX_NATS_TOKEN:-}}" ]                  || [ -n "${{HARNX_SERVER_SCOPE:-}}" ]; then exit 3; fi
               exec {} --mcp-stdio"#,
            binary.display()
        );
        let bridge =
            BridgeToolset::new("scrubbed", vec!["sh".to_string(), "-c".to_string(), guard])
                .await
                .expect("child starts only when the worker identity is scrubbed");

        assert!(!bridge.cached_tools().is_empty());
    }

    #[cfg(unix)]
    fn plans_tools_binary() -> Option<std::path::PathBuf> {
        let mut dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
        if dir.file_name().is_some_and(|name| name == "deps") {
            dir.pop();
        }
        let binary = dir.join("harnx-plans-tools");
        binary.is_file().then_some(binary)
    }

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

        assert_eq!(args.name.as_deref(), Some("plans"));
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

        assert_eq!(args.name.as_deref(), Some("plans"));
        assert_eq!(args.child, ["harnx-plans-tools"]);
    }

    #[test]
    fn maps_child_tool_meta_into_tool_spec() {
        let meta = rmcp::model::MetaObject(
            serde_json::json!({ "call_template": "Running {{name}}" })
                .as_object()
                .expect("meta object")
                .clone(),
        );
        let tool =
            rmcp::model::Tool::new("echo", "Echo input", serde_json::Map::new()).with_meta(meta);

        let spec = map_tool(tool);

        assert_eq!(spec.name, "echo");
        assert_eq!(
            spec.meta
                .as_ref()
                .and_then(|meta| meta.get("call_template"))
                .and_then(serde_json::Value::as_str),
            Some("Running {{name}}")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn caches_raw_plans_tool_specs() {
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
        assert!(bridge
            .cached_tools()
            .iter()
            .any(|tool| tool.name == "list_plans"));
        assert!(bridge.cached_tools().iter().all(|tool| {
            !tool.name.starts_with("plans_")
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
    async fn invokes_raw_child_tools_and_validates_arguments_and_cancellation() {
        use harnx_toolset::{ToolInvokeError, Toolset};
        use tokio_util::sync::CancellationToken;

        let plans_dir = tempfile::tempdir().expect("create temporary plans directory");
        let bridge = BridgeToolset::new("plans", plans_command(plans_dir.path()))
            .await
            .expect("connect to plans MCP server");

        let result = bridge
            .invoke(
                "list_plans",
                serde_json::json!({}),
                CancellationToken::new(),
            )
            .await
            .expect("invoke plans list_plans");
        assert!(result
            .get("content")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|content| !content.is_empty()));

        let bad_arguments = bridge
            .invoke(
                "list_plans",
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
            .invoke("list_plans", serde_json::json!({}), cancel)
            .await;
        assert!(matches!(
            cancelled,
            Err(ToolInvokeError::Recoverable(message)) if message == "call cancelled"
        ));
    }
}

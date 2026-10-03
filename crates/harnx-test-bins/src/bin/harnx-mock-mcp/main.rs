//! Deterministic, script-driven mock MCP server for demo recordings.
//!
//! Usage: `harnx-mock-mcp --script <path/to/script.yaml>`
//!
//! See `server.rs` for the script format. When no script is given, a tiny
//! built-in default is used.

mod launcher;
mod server;

use rmcp::ServiceExt;
use server::MockMcpServer;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

const DEFAULT_SCRIPT: &str = r#"
tools:
  - name: echo
    description: Echo a canned response.
    call_template: "echo {{ args.text }}"
responses:
  - "Hello from harnx-mock-mcp."
fallback: "(no more scripted responses)"
"#;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = parse_args()?;
    // Keep this OS lock held even after MCP transport closure. Tests can prove
    // termination of the exact wrapped process, rather than only stdio closure.
    let _lifetime_lock = acquire_lifetime_lock(args.lifetime_lock.as_deref())?;
    #[cfg(unix)]
    if args.linger {
        // Parent-death SIGTERM must not conceal a broken explicit bridge cleanup.
        unsafe {
            libc::signal(libc::SIGTERM, libc::SIG_IGN);
        }
    }
    if let Some(path) = &args.spawn_log {
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        writeln!(file, "{}", std::process::id())?;
    }
    if let Some(dir) = &args.launcher_dir {
        return launcher::run(&args, dir).await;
    }
    serve_mock(args).await
}

async fn serve_mock(args: Args) -> anyhow::Result<()> {
    if let Some(gate) = &args.start_gate {
        while !gate.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
    let yaml = match args.script_path {
        Some(path) => std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("failed to read script '{path}': {e}"))?,
        None => DEFAULT_SCRIPT.to_string(),
    };
    let server = MockMcpServer::from_script_str(&yaml)?.with_request_log(args.request_log);

    eprintln!("harnx-mock-mcp v{}: starting", env!("CARGO_PKG_VERSION"));
    let result = match server.serve(rmcp::transport::stdio()).await {
        Ok(service) => service.waiting().await.map(|_| ()).map_err(Into::into),
        Err(error) => Err(error.into()),
    };
    if args.linger {
        // Failure backstop for a broken bridge/test, not the expected cleanup path.
        tokio::time::sleep(std::time::Duration::from_secs(120)).await;
    }
    result
}

fn acquire_lifetime_lock(path: Option<&std::path::Path>) -> anyhow::Result<Option<std::fs::File>> {
    let Some(path) = path else { return Ok(None) };
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    file.lock()?;
    Ok(Some(file))
}

struct Args {
    script_path: Option<String>,
    spawn_log: Option<PathBuf>,
    request_log: Option<PathBuf>,
    lifetime_lock: Option<PathBuf>,
    start_gate: Option<PathBuf>,
    linger: bool,
    launcher_dir: Option<PathBuf>,
    launcher_exit: bool,
}

fn parse_args() -> anyhow::Result<Args> {
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    let mut script_path = None;
    let mut spawn_log = None;
    let mut request_log = None;
    let mut lifetime_lock = None;
    let mut start_gate = None;
    let mut linger = false;
    let mut launcher_dir = None;
    let mut launcher_exit = false;
    while i < args.len() {
        match args[i].as_str() {
            "--script" => {
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow::anyhow!("--script requires a path argument"))?;
                script_path = Some(value.clone());
                i += 2;
            }
            "--spawn-log" => {
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow::anyhow!("--spawn-log requires a path argument"))?;
                spawn_log = Some(PathBuf::from(value));
                i += 2;
            }
            "--request-log" | "--lifetime-lock" | "--start-gate" | "--launcher-dir" => {
                let value = args.get(i + 1).ok_or_else(|| anyhow::anyhow!("{} requires a path argument", args[i]))?;
                match args[i].as_str() {
                    "--request-log" => request_log = Some(PathBuf::from(value)),
                    "--lifetime-lock" => lifetime_lock = Some(PathBuf::from(value)),
                    "--launcher-dir" => launcher_dir = Some(PathBuf::from(value)),
                    _ => start_gate = Some(PathBuf::from(value)),
                }
                i += 2;
            }
            "--linger" => { linger = true; i += 1; }
            "--launcher-exit" => { launcher_exit = true; i += 1; }
            other => anyhow::bail!(
                "unknown argument: {other}; usage: harnx-mock-mcp [--script <path>] [--spawn-log <path>]"
            ),
        }
    }
    Ok(Args {
        script_path,
        spawn_log,
        request_log,
        lifetime_lock,
        start_gate,
        linger,
        launcher_dir,
        launcher_exit,
    })
}

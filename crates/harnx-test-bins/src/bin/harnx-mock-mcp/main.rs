//! Deterministic, script-driven mock MCP server for demo recordings.
//!
//! Usage: `harnx-mock-mcp --script <path/to/script.yaml>`
//!
//! See `server.rs` for the script format. When no script is given, a tiny
//! built-in default is used.

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
    if let Some(path) = &args.spawn_log {
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        writeln!(file, "{}", std::process::id())?;
    }
    let yaml = match args.script_path {
        Some(path) => std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("failed to read script '{path}': {e}"))?,
        None => DEFAULT_SCRIPT.to_string(),
    };
    let server = MockMcpServer::from_script_str(&yaml)?;

    eprintln!("harnx-mock-mcp v{}: starting", env!("CARGO_PKG_VERSION"));
    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

struct Args {
    script_path: Option<String>,
    spawn_log: Option<PathBuf>,
}

fn parse_args() -> anyhow::Result<Args> {
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    let mut script_path = None;
    let mut spawn_log = None;
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
            other => anyhow::bail!(
                "unknown argument: {other}; usage: harnx-mock-mcp [--script <path>] [--spawn-log <path>]"
            ),
        }
    }
    Ok(Args {
        script_path,
        spawn_log,
    })
}

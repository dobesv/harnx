//! Deterministic, script-driven mock MCP server for demo recordings.
//!
//! Usage: `harnx-mock-mcp --script <path/to/script.yaml>`
//!
//! See `server.rs` for the script format. When no script is given, a tiny
//! built-in default is used.

mod server;

use rmcp::ServiceExt;
use server::MockMcpServer;

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
    let script_path = parse_script_arg()?;
    let yaml = match script_path {
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

fn parse_script_arg() -> anyhow::Result<Option<String>> {
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    let mut script = None;
    while i < args.len() {
        match args[i].as_str() {
            "--script" => {
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow::anyhow!("--script requires a path argument"))?;
                script = Some(value.clone());
                i += 2;
            }
            other => anyhow::bail!("unknown argument: {other}"),
        }
    }
    Ok(script)
}

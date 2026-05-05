//! harnx-mcp-plans: File-based plan/task/note management MCP server.
//!
//! Stores plans, tasks, and notes as YAML-frontmatter markdown files in per-plan
//! subdirectories under `.agent/plans/` (configurable via `--dir` or `AGENT_PLANS_PATH`).
//!
//! Layout: `<dir>/<plan>/plan.md`, `<dir>/<plan>/tasks/<id>.md`, `<dir>/<plan>/notes/<id>.md`
//!
//! Provides: list_plans, add_plan, get_plan, update_plan, delete_plan,
//! list_tasks, add_task, get_task, update_task, append_task, delete_task,
//! list_notes, add_note, get_note, delete_note

mod server;

use rmcp::ServiceExt;
use server::PlansServer;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let plans_dir = parse_args();

    eprintln!(
        "harnx-mcp-plans v{}: starting (dir: {})",
        env!("CARGO_PKG_VERSION"),
        plans_dir.display()
    );

    let server = PlansServer::new(plans_dir);
    let transport = rmcp::transport::stdio();
    let service = server.serve(transport).await?;
    service.waiting().await?;

    Ok(())
}

fn parse_args() -> PathBuf {
    let args: Vec<String> = std::env::args().collect();
    let mut plans_dir: Option<PathBuf> = None;
    let mut i = 1;

    while i < args.len() {
        match args[i].as_str() {
            "--dir" | "-d" => {
                if i + 1 < args.len() {
                    plans_dir = Some(PathBuf::from(&args[i + 1]));
                    i += 2;
                } else {
                    eprintln!("harnx-mcp-plans: --dir requires a path argument");
                    std::process::exit(1);
                }
            }
            "--help" | "-h" => {
                eprintln!("harnx-mcp-plans: File-based todo/plan management MCP server");
                eprintln!();
                eprintln!("Usage: harnx-mcp-plans [OPTIONS]");
                eprintln!();
                eprintln!("Options:");
                eprintln!("  --dir, -d <path>  Set the plans directory (default: .agent/plans)");
                eprintln!("  --help, -h        Show this help message");
                eprintln!();
                eprintln!("Env: AGENT_PLANS_PATH overrides the default directory.");
                std::process::exit(0);
            }
            other => {
                eprintln!("harnx-mcp-plans: unknown argument: {}", other);
                eprintln!("Try: harnx-mcp-plans --help");
                std::process::exit(1);
            }
        }
    }

    if let Some(dir) = plans_dir {
        return dir;
    }

    if let Ok(env_path) = std::env::var("AGENT_PLANS_PATH") {
        if !env_path.trim().is_empty() {
            return PathBuf::from(env_path.trim());
        }
    }

    PathBuf::from(".agent/plans")
}

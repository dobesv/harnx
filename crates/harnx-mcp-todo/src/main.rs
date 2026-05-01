//! harnx-mcp-todo: File-based todo/plan management MCP server.
//!
//! Stores todos as YAML-frontmatter markdown files in per-plan subdirectories
//! under `.agent/todos/` (configurable via `--dir` or `AGENT_TODO_PATH` env var).
//!
//! Layout: `<dir>/<plan>/plan.md` and `<dir>/<plan>/todo-<id>.md`
//!
//! Provides: todo_list, todo_get, todo_create, todo_update, todo_append,
//! todo_delete, read_plan, write_plan, plan_add_note, plan_get_todo

mod server;

use rmcp::ServiceExt;
use server::TodoServer;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let todo_dir = parse_args();

    eprintln!(
        "harnx-mcp-todo v{}: starting (dir: {})",
        env!("CARGO_PKG_VERSION"),
        todo_dir.display()
    );

    let server = TodoServer::new(todo_dir);
    let transport = rmcp::transport::stdio();
    let service = server.serve(transport).await?;
    service.waiting().await?;

    Ok(())
}

fn parse_args() -> PathBuf {
    let args: Vec<String> = std::env::args().collect();
    let mut todo_dir: Option<PathBuf> = None;
    let mut i = 1;

    while i < args.len() {
        match args[i].as_str() {
            "--dir" | "-d" => {
                if i + 1 < args.len() {
                    todo_dir = Some(PathBuf::from(&args[i + 1]));
                    i += 2;
                } else {
                    eprintln!("harnx-mcp-todo: --dir requires a path argument");
                    std::process::exit(1);
                }
            }
            "--help" | "-h" => {
                eprintln!("harnx-mcp-todo: File-based todo/plan management MCP server");
                eprintln!();
                eprintln!("Usage: harnx-mcp-todo [OPTIONS]");
                eprintln!();
                eprintln!("Options:");
                eprintln!("  --dir, -d <path>  Set the todos directory (default: .agent/todos)");
                eprintln!("  --help, -h        Show this help message");
                eprintln!();
                eprintln!("Env: AGENT_TODO_PATH overrides the default directory.");
                std::process::exit(0);
            }
            other => {
                eprintln!("harnx-mcp-todo: unknown argument: {}", other);
                eprintln!("Try: harnx-mcp-todo --help");
                std::process::exit(1);
            }
        }
    }

    if let Some(dir) = todo_dir {
        return dir;
    }

    if let Ok(env_path) = std::env::var("AGENT_TODO_PATH") {
        if !env_path.trim().is_empty() {
            return PathBuf::from(env_path.trim());
        }
    }

    PathBuf::from(".agent/todos")
}

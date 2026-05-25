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
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (plans_dir, retention_days) = parse_args()?;

    eprintln!(
        "harnx-mcp-plans v{}: starting (dir: {}, retention: {} days)",
        env!("CARGO_PKG_VERSION"),
        plans_dir.display(),
        retention_days
    );

    let server = PlansServer::new(plans_dir.clone());
    let transport = rmcp::transport::stdio();
    let service = server.serve(transport).await?;

    if retention_days == 0 {
        eprintln!("[cleanup] retention disabled");
        service.waiting().await?;
    } else {
        let cleanup_dir = plans_dir.clone();
        let mut cleanup_handle = tokio::spawn(server::cleanup_loop(plans_dir, retention_days));
        let service_handle = tokio::spawn(async move { service.waiting().await });
        tokio::pin!(service_handle);

        // Exponential backoff state for cleanup task restarts.
        // Reset to base on a clean exit; doubles on each panic up to MAX_BACKOFF.
        const BASE_BACKOFF: Duration = Duration::from_secs(1);
        const MAX_BACKOFF: Duration = Duration::from_secs(300); // 5 min cap
        let mut backoff = BASE_BACKOFF;

        loop {
            tokio::select! {
                result = &mut *service_handle => {
                    // MCP service exited — normal shutdown
                    result??;
                    break;
                }
                result = &mut cleanup_handle => {
                    match result {
                        Err(e) => {
                            // Task panicked — log, back off, restart
                            eprintln!("[cleanup] task failed: {e}");
                            tokio::time::sleep(backoff).await;
                            backoff = (backoff * 2).min(MAX_BACKOFF);
                        }
                        Ok(()) => {
                            // Clean exit (shouldn't happen in normal operation) — restart immediately
                            backoff = BASE_BACKOFF;
                        }
                    }
                    cleanup_handle = tokio::spawn(server::cleanup_loop(cleanup_dir.clone(), retention_days));
                }
            }
        }
    }

    Ok(())
}

fn parse_args() -> anyhow::Result<(PathBuf, u64)> {
    let args: Vec<String> = std::env::args().collect();
    let mut plans_dir: Option<PathBuf> = None;
    let mut retention_days: Option<u64> = None;
    let mut i = 1;

    while i < args.len() {
        match args[i].as_str() {
            "--dir" | "-d" => {
                if i + 1 < args.len() {
                    plans_dir = Some(PathBuf::from(&args[i + 1]));
                    i += 2;
                } else {
                    anyhow::bail!("harnx-mcp-plans: --dir requires a path argument");
                }
            }
            "--retention-days" | "-r" => {
                if i + 1 < args.len() {
                    match args[i + 1].parse::<u64>() {
                        Ok(days) => {
                            retention_days = Some(days);
                            i += 2;
                        }
                        Err(_) => {
                            anyhow::bail!(
                                "harnx-mcp-plans: --retention-days requires a non-negative integer (got: {})",
                                args[i + 1]
                            );
                        }
                    }
                } else {
                    anyhow::bail!("harnx-mcp-plans: --retention-days requires a number argument");
                }
            }
            "--help" | "-h" => {
                eprintln!("harnx-mcp-plans: File-based todo/plan management MCP server");
                eprintln!();
                eprintln!("Usage: harnx-mcp-plans [OPTIONS]");
                eprintln!();
                eprintln!("Options:");
                eprintln!(
                    "  --dir, -d <path>           Set the plans directory (default: .agent/plans)"
                );
                eprintln!(
                    "  --retention-days, -r <N>   Set retention period in days (default: 14)"
                );
                eprintln!("  --help, -h                 Show this help message");
                eprintln!();
                eprintln!("Env:");
                eprintln!("  AGENT_PLANS_PATH         Overrides the default directory.");
                eprintln!("  AGENT_PLANS_RETENTION_DAYS  Overrides the default retention days.");
                std::process::exit(0);
            }
            other => {
                anyhow::bail!(
                    "harnx-mcp-plans: unknown argument: {other}\nTry: harnx-mcp-plans --help"
                );
            }
        }
    }

    // Resolve retention_days: CLI flag > env var > default (14)
    let retention_days = if let Some(days) = retention_days {
        days
    } else if let Ok(env_days) = std::env::var("AGENT_PLANS_RETENTION_DAYS") {
        match env_days.trim().parse::<u64>() {
            Ok(days) => days,
            Err(_) => {
                anyhow::bail!(
                    "harnx-mcp-plans: AGENT_PLANS_RETENTION_DAYS must be a non-negative integer (got: {})",
                    env_days.trim()
                );
            }
        }
    } else {
        14
    };

    // Resolve plans_dir: CLI flag > env var > default
    let plans_dir = if let Some(dir) = plans_dir {
        dir
    } else if let Ok(env_path) = std::env::var("AGENT_PLANS_PATH") {
        if !env_path.trim().is_empty() {
            PathBuf::from(env_path.trim())
        } else {
            PathBuf::from(".agent/plans")
        }
    } else {
        PathBuf::from(".agent/plans")
    };

    Ok((plans_dir, retention_days))
}

//! harnx-fs-tools: Filesystem toolset server, with MCP stdio back-compat.

use harnx_fs_tools::FsToolset;
use harnx_tool_allow::{resolve_allowlist, AllowEnv, AllowInputs};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = harnx_core::logging::init(harnx_core::logging::LogSink::Stderr);
    let inputs = parse_args();
    let cwd = std::env::current_dir()?;
    let allowlist = resolve_allowlist(&inputs, &cwd, &AllowEnv::from_current_process());

    log::info!(
        "harnx-fs-tools v{}: starting ({} read, {} write allow path{})",
        env!("CARGO_PKG_VERSION"),
        allowlist.read_paths().len(),
        allowlist.write_paths().len(),
        if allowlist.write_paths().len() == 1 {
            ""
        } else {
            "s"
        }
    );

    let toolset = FsToolset::new(allowlist);
    harnx_toolset_server::run_toolset_main(toolset).await
}

fn env_paths(name: &str) -> Vec<PathBuf> {
    std::env::var_os(name)
        .map(|value| {
            std::env::split_paths(&value)
                .filter(|path| !path.as_os_str().is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn env_toggle(name: &str) -> bool {
    std::env::var(name)
        .map(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

/// Parse filesystem-specific CLI arguments. Shared server runner consumes
/// `--mcp-stdio`, so this parser accepts it without changing allowlist setup.
fn parse_args() -> AllowInputs {
    let args: Vec<String> = std::env::args().collect();
    parse_args_from(&args, initial_allow_inputs()).unwrap_or_else(|error| {
        eprintln!("{error}");
        std::process::exit(1);
    })
}

fn initial_allow_inputs() -> AllowInputs {
    AllowInputs {
        read: env_paths("HARNX_TOOLS_ALLOW_READ"),
        write: env_paths("HARNX_TOOLS_ALLOW_WRITE"),
        exec: env_paths("HARNX_TOOLS_ALLOW_EXEC"),
        rwx: env_paths("HARNX_TOOLS_ALLOW_RWX"),
        common_default: env_toggle("HARNX_TOOLS_ALLOW_COMMON_DEFAULT"),
        dev_tools: env_toggle("HARNX_TOOLS_ALLOW_DEV_TOOLS"),
        repo_work: env_toggle("HARNX_TOOLS_ALLOW_REPO_WORK"),
        all: env_toggle("HARNX_TOOLS_ALLOW_ALL"),
    }
}

fn parse_args_from(args: &[String], mut inputs: AllowInputs) -> Result<AllowInputs, String> {
    let mut i = 1;

    while i < args.len() {
        let target = match args[i].as_str() {
            "--allow-read" => Some(&mut inputs.read),
            "--allow-write" => Some(&mut inputs.write),
            "--allow-exec" => Some(&mut inputs.exec),
            "--allow-rwx" => Some(&mut inputs.rwx),
            "--allow-common-default" => {
                inputs.common_default = true;
                None
            }
            "--allow-dev-tools" => {
                inputs.dev_tools = true;
                None
            }
            "--allow-repo-work" => {
                inputs.repo_work = true;
                None
            }
            "--allow-all" => {
                inputs.all = true;
                None
            }
            arg if arg.starts_with("--metrics-addr") => {
                // Skip --metrics-addr (both forms) so the strict parser doesn't reject it.
                // The shared helper in run_toolset_main handles actual parsing.
                if arg == "--metrics-addr" {
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }
            "--mcp-stdio" => None,
            "--help" | "-h" => print_help_and_exit(),
            other => {
                return Err(format!(
                    "harnx-fs-tools: unknown argument: {other}\nTry: harnx-fs-tools --help"
                ));
            }
        };

        if let Some(paths) = target {
            let path = args
                .get(i + 1)
                .ok_or_else(|| format!("harnx-fs-tools: {} requires a path argument", args[i]))?;
            paths.push(PathBuf::from(path));
            i += 2;
        } else {
            i += 1;
        }
    }

    Ok(inputs)
}

fn print_help_and_exit() -> ! {
    eprintln!("harnx-fs-tools: Filesystem toolset server");
    eprintln!();
    eprintln!("Usage: harnx-fs-tools [OPTIONS]");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --allow-read <path>       Allow filesystem reads (repeatable)");
    eprintln!("  --allow-write <path>      Allow reads and writes (repeatable)");
    eprintln!("  --allow-exec <path>       Allow reads; fs has no exec operation (repeatable)");
    eprintln!("  --allow-rwx <path>        Allow reads and writes (repeatable)");
    eprintln!("  --allow-common-default    Allow common operating-system paths");
    eprintln!("  --allow-dev-tools         Allow development tool paths");
    eprintln!("  --allow-repo-work         Allow detected project roots and current directory");
    eprintln!("  --allow-all               Allow all filesystem paths");
    eprintln!("  --mcp-stdio               Serve MCP over stdio instead of toolset mode");
    eprintln!("  --metrics-addr <ADDR>     Serve Prometheus metrics at http://ADDR/metrics.");
    eprintln!("                            Blank host binds 0.0.0.0, e.g. :8456. Unset disables.");
    eprintln!("                            Also honors HARNX_METRICS_ADDR env.");
    eprintln!("  --help, -h                Show this help message");
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_legacy_allowlist_flags() {
        let legacy_flags = [
            ["--", "root"].concat(),
            ["--extra", "-read"].concat(),
            ["--default", "-root", "-cwd"].concat(),
        ];

        for flag in legacy_flags {
            let args = vec!["harnx-fs-tools".to_string(), flag.clone()];
            let error = parse_args_from(&args, AllowInputs::default())
                .expect_err("legacy flag should be rejected");
            assert!(error.contains(&format!("unknown argument: {flag}")));
        }
    }
}

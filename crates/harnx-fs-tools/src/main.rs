//! harnx-fs-tools: Filesystem toolset server, with MCP stdio back-compat.

use harnx_fs_tools::FsToolset;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (roots, default_root_cwd) = parse_args();

    eprintln!(
        "harnx-fs-tools v{}: starting ({} CLI root{})",
        env!("CARGO_PKG_VERSION"),
        roots.len(),
        if roots.len() == 1 { "" } else { "s" }
    );
    for root in &roots {
        eprintln!("  root: {}", root.display());
    }

    let toolset = FsToolset::new(roots, default_root_cwd).await;
    harnx_toolset_server::run_toolset_main(toolset).await
}

/// Parse filesystem-specific CLI arguments. The shared server runner consumes
/// `--mcp-stdio`, so this parser accepts it without changing filesystem setup.
fn parse_args() -> (Vec<PathBuf>, bool) {
    let args: Vec<String> = std::env::args().collect();
    let mut roots = Vec::new();
    let mut default_root_cwd = false;
    let mut i = 1;

    while i < args.len() {
        match args[i].as_str() {
            "--root" | "-r" => {
                if i + 1 < args.len() {
                    let raw = &args[i + 1];
                    let path = PathBuf::from(raw);
                    if path.exists() {
                        match path.canonicalize() {
                            Ok(canonical) => roots.push(canonical),
                            Err(err) => {
                                eprintln!(
                                    "harnx-fs-tools: warning: failed to canonicalize root '{}': {}",
                                    raw, err
                                );
                            }
                        }
                    } else {
                        eprintln!("harnx-fs-tools: warning: root path does not exist: {}", raw);
                    }
                    i += 2;
                } else {
                    eprintln!("harnx-fs-tools: --root requires a path argument");
                    std::process::exit(1);
                }
            }
            "--default-root-cwd" => {
                default_root_cwd = true;
                i += 1;
            }
            "--mcp-stdio" => {
                i += 1;
            }
            "--help" | "-h" => {
                eprintln!("harnx-fs-tools: Filesystem toolset server");
                eprintln!();
                eprintln!("Usage: harnx-fs-tools [OPTIONS]");
                eprintln!();
                eprintln!("Options:");
                eprintln!("  --root, -r <path>   Add an allowed root directory (repeatable)");
                eprintln!("  --default-root-cwd  Use CWD when no other roots are available");
                eprintln!("  --mcp-stdio         Serve MCP over stdio instead of the default toolset mode");
                eprintln!("  --help, -h          Show this help message");
                std::process::exit(0);
            }
            other => {
                eprintln!("harnx-fs-tools: unknown argument: {}", other);
                eprintln!("Try: harnx-fs-tools --help");
                std::process::exit(1);
            }
        }
    }

    (roots, default_root_cwd)
}

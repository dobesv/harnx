#[cfg(test)]
mod test_support;

use harnx_bash_tools::{discover_tool_templates, BashToolset, ToolTemplate};
use harnx_sandbox_common::SandboxConfig;
use harnx_tool_allow::{resolve_allowlist, AllowEnv, AllowInputs};
use std::path::PathBuf;
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = harnx_core::logging::init(harnx_core::logging::LogSink::Stderr);
    let (sandbox_config, templates) = parse_args()?;
    let allowlist = &sandbox_config.allowlist;

    log::info!(
        "harnx-bash-tools v{}: starting ({} read, {} write, {} exec allow paths, {} command templates)",
        env!("CARGO_PKG_VERSION"),
        allowlist.read_paths().len(),
        allowlist.write_paths().len(),
        allowlist.exec_paths().len(),
        templates.len(),
    );

    #[cfg(unix)]
    if sandbox_config.enabled {
        log::info!(
            "  sandbox: enabled (helper: {})",
            sandbox_config.sandbox_run_path.display()
        );
    } else {
        log::info!("  sandbox: disabled");
    }

    let toolset = BashToolset::new(sandbox_config, templates).await?;
    let cleanup_toolset = toolset.clone();
    let result = harnx_toolset_server::run_toolset_main(toolset).await;
    if let Err(err) = cleanup_toolset.cleanup_log_dir() {
        log::warn!("failed to clean temp log dir: {err}");
    }
    result
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

#[cfg(unix)]
fn path_is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    path.metadata()
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn parse_env_passthrough() -> Vec<String> {
    std::env::var("HARNX_BASH_ENV_PASSTHROUGH")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
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

fn parse_env_option(args: &[String], i: &mut usize, config: &mut SandboxConfig) {
    let Some(raw) = args.get(*i + 1) else {
        eprintln!("harnx-bash-tools: --env requires an argument");
        std::process::exit(1);
    };

    if let Some((key, value)) = raw.split_once('=') {
        if key.is_empty() {
            eprintln!("harnx-bash-tools: --env requires a non-empty variable name");
            std::process::exit(1);
        }
        config
            .env_overrides
            .push((key.to_string(), value.to_string()));
    } else {
        if raw.is_empty() {
            eprintln!("harnx-bash-tools: --env requires a non-empty variable name");
            std::process::exit(1);
        }
        config.extra_env_passthrough.push(raw.clone());
    }
    *i += 2;
}

fn required_path(args: &[String], i: usize) -> PathBuf {
    args.get(i + 1).map(PathBuf::from).unwrap_or_else(|| {
        eprintln!("harnx-bash-tools: {} requires a path argument", args[i]);
        std::process::exit(1);
    })
}

fn print_help_and_exit() -> ! {
    eprintln!("harnx-bash-tools: MCP shell command server");
    eprintln!();
    eprintln!("Usage: harnx-bash-tools [OPTIONS]");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --allow-read <path>       Allow filesystem reads (repeatable)");
    eprintln!("  --allow-write <path>      Allow filesystem reads and writes (repeatable)");
    eprintln!("  --allow-exec <path>       Allow filesystem reads and execution (repeatable)");
    eprintln!("  --allow-rwx <path>        Allow reads, writes, and execution (repeatable)");
    eprintln!("  --allow-common-default    Allow common operating-system paths");
    eprintln!("  --allow-dev-tools         Allow development tool paths");
    eprintln!("  --allow-repo-work         Allow detected project roots and current directory");
    eprintln!("  --allow-all               Allow all filesystem paths (HOME guard still applies)");
    eprintln!("  --no-sandbox              Disable filesystem sandboxing explicitly");
    eprintln!("  --sandbox-run <path>      Override sandbox helper binary path");
    eprintln!("  --tool <path>             Load a command template YAML file (repeatable)");
    eprintln!("  --tools-dir <path>        Load command template YAML files from a directory (repeatable)");
    eprintln!("  --env, -e <VAR>           Pass VAR from host env to child (repeatable)");
    eprintln!("  --env, -e <VAR=VALUE>     Set VAR=VALUE in child env (repeatable)");
    eprintln!("  --mcp-stdio               Use MCP stdio transport instead of NATS");
    eprintln!("  --metrics-addr <ADDR>     Serve Prometheus metrics at http://ADDR/metrics.");
    eprintln!("                            Blank host binds 0.0.0.0, e.g. :8456. Unset disables.");
    eprintln!("                            Also honors HARNX_METRICS_ADDR env.");
    eprintln!("  --help, -h                Show this help message");
    eprintln!();
    eprintln!("Environment:");
    eprintln!("  HARNX_TOOLS_ALLOW_READ            Path-list of read grants");
    eprintln!("  HARNX_TOOLS_ALLOW_WRITE           Path-list of read/write grants");
    eprintln!("  HARNX_TOOLS_ALLOW_EXEC            Path-list of read/exec grants");
    eprintln!("  HARNX_TOOLS_ALLOW_RWX             Path-list of read/write/exec grants");
    eprintln!("  HARNX_TOOLS_ALLOW_COMMON_DEFAULT  Enable common-default batch (1/true/yes/on)");
    eprintln!("  HARNX_TOOLS_ALLOW_DEV_TOOLS       Enable dev-tools batch (1/true/yes/on)");
    eprintln!("  HARNX_TOOLS_ALLOW_REPO_WORK       Enable repo-work batch (1/true/yes/on)");
    eprintln!("  HARNX_TOOLS_ALLOW_ALL             Enable allow-all batch (1/true/yes/on)");
    eprintln!("  HARNX_BASH_ENV_PASSTHROUGH        Comma-separated extra child env names");
    eprintln!("  HARNX_PACKAGE_DIR                 Package directory containing bash_tools/");
    eprintln!();
    eprintln!("No allow flags or batch toggles means deny-all filesystem access.");
    #[cfg(not(unix))]
    eprintln!("Sandboxing is Unix-only; filesystem allow inputs are not enforced here.");
    std::process::exit(0);
}

fn initial_sandbox_config() -> SandboxConfig {
    SandboxConfig {
        enabled: cfg!(unix),
        allowlist: Arc::new(harnx_tool_allow::ResolvedAllowlist::new()),
        extra_env_passthrough: parse_env_passthrough(),
        env_overrides: Vec::new(),
        sandbox_run_path: PathBuf::from("harnx-sandbox-exec"),
    }
}

fn parse_cli_args(
    args: &[String],
    inputs: &mut AllowInputs,
    config: &mut SandboxConfig,
    cli_files: &mut Vec<PathBuf>,
    cli_dirs: &mut Vec<PathBuf>,
) -> Result<Option<PathBuf>, String> {
    let mut sandbox_run_override = None;
    let mut i = 1;

    while i < args.len() {
        match args[i].as_str() {
            "--allow-read" => {
                inputs.read.push(required_path(args, i));
                i += 2;
            }
            "--allow-write" => {
                inputs.write.push(required_path(args, i));
                i += 2;
            }
            "--allow-exec" => {
                inputs.exec.push(required_path(args, i));
                i += 2;
            }
            "--allow-rwx" => {
                inputs.rwx.push(required_path(args, i));
                i += 2;
            }
            "--allow-common-default" => {
                inputs.common_default = true;
                i += 1;
            }
            "--allow-dev-tools" => {
                inputs.dev_tools = true;
                i += 1;
            }
            "--allow-repo-work" => {
                inputs.repo_work = true;
                i += 1;
            }
            "--allow-all" => {
                inputs.all = true;
                i += 1;
            }
            "--no-sandbox" => {
                config.enabled = false;
                i += 1;
            }
            "--sandbox-run" => {
                sandbox_run_override = Some(required_path(args, i));
                i += 2;
            }
            "--tool" => {
                cli_files.push(required_path(args, i));
                i += 2;
            }
            "--tools-dir" => {
                cli_dirs.push(required_path(args, i));
                i += 2;
            }
            "--env" | "-e" => parse_env_option(args, &mut i, config),
            arg if arg.starts_with("--metrics-addr") => {
                // Skip --metrics-addr (both forms) so the strict parser doesn't reject it.
                // The shared helper in run_toolset_main handles actual parsing.
                if arg == "--metrics-addr" {
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "--mcp-stdio" => i += 1,
            "--help" | "-h" => print_help_and_exit(),
            other => {
                return Err(format!(
                    "harnx-bash-tools: unknown argument: {other}\nTry: harnx-bash-tools --help"
                ));
            }
        }
    }

    Ok(sandbox_run_override)
}

fn parse_args() -> anyhow::Result<(SandboxConfig, Vec<ToolTemplate>)> {
    let args: Vec<String> = std::env::args().collect();
    let cwd = std::env::current_dir()?;
    let mut inputs = initial_allow_inputs();
    let mut config = initial_sandbox_config();
    let mut cli_files = Vec::new();
    let mut cli_dirs = Vec::new();
    let sandbox_run_override = parse_cli_args(
        &args,
        &mut inputs,
        &mut config,
        &mut cli_files,
        &mut cli_dirs,
    )
    .unwrap_or_else(|error| {
        eprintln!("{error}");
        std::process::exit(1);
    });

    #[cfg(unix)]
    {
        let resolved_helper = sandbox_run_override.clone().or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|path| path.parent().map(|dir| dir.join("harnx-sandbox-exec")))
        });
        if config.enabled {
            let helper = resolved_helper.unwrap_or_else(|| PathBuf::from("harnx-sandbox-exec"));
            if !path_is_executable(&helper) {
                anyhow::bail!(
                    "harnx-bash-tools: error: sandbox helper at {} does not exist or is not executable; fix --sandbox-run or pass --no-sandbox to disable sandboxing explicitly",
                    helper.display()
                );
            }
            config.sandbox_run_path = helper;
        } else if let Some(helper) = resolved_helper {
            config.sandbox_run_path = helper;
        }
    }
    #[cfg(not(unix))]
    if let Some(helper) = sandbox_run_override {
        config.sandbox_run_path = helper;
    }

    config.allowlist = Arc::new(resolve_allowlist(
        &inputs,
        &cwd,
        &AllowEnv::from_current_process(),
    ));

    let package_dir = std::env::var_os("HARNX_PACKAGE_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let templates = discover_tool_templates(package_dir.as_deref(), &cli_files, &cli_dirs)?;
    Ok((config, templates))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{env_lock, EnvVar};

    #[cfg(unix)]
    #[test]
    fn shared_allow_environment_names_are_parsed() {
        let _guard = env_lock();
        let _read = EnvVar::set("HARNX_TOOLS_ALLOW_READ", "/read:/other");
        let _write = EnvVar::set("HARNX_TOOLS_ALLOW_WRITE", "/write");
        let _exec = EnvVar::set("HARNX_TOOLS_ALLOW_EXEC", "/exec");
        let _rwx = EnvVar::set("HARNX_TOOLS_ALLOW_RWX", "/rwx");
        let _batch = EnvVar::set("HARNX_TOOLS_ALLOW_COMMON_DEFAULT", "true");

        let inputs = initial_allow_inputs();
        assert_eq!(
            inputs.read,
            [PathBuf::from("/read"), PathBuf::from("/other")]
        );
        assert_eq!(inputs.write, [PathBuf::from("/write")]);
        assert_eq!(inputs.exec, [PathBuf::from("/exec")]);
        assert_eq!(inputs.rwx, [PathBuf::from("/rwx")]);
        assert!(inputs.common_default);
    }

    #[test]
    fn false_batch_toggle_stays_disabled() {
        let _guard = env_lock();
        let _batch = EnvVar::set("HARNX_TOOLS_ALLOW_ALL", "false");
        assert!(!env_toggle("HARNX_TOOLS_ALLOW_ALL"));
    }

    #[test]
    fn repeated_template_flags_collect_paths() {
        let args = [
            "harnx-bash-tools",
            "--tool",
            "one.yaml",
            "--tools-dir",
            "templates",
            "--tool",
            "two.yaml",
        ]
        .map(str::to_string);
        let mut inputs = AllowInputs::default();
        let mut config = initial_sandbox_config();
        let mut cli_files = Vec::new();
        let mut cli_dirs = Vec::new();

        parse_cli_args(
            &args,
            &mut inputs,
            &mut config,
            &mut cli_files,
            &mut cli_dirs,
        )
        .expect("template flags should parse");

        assert_eq!(
            cli_files,
            [PathBuf::from("one.yaml"), PathBuf::from("two.yaml")]
        );
        assert_eq!(cli_dirs, [PathBuf::from("templates")]);
    }

    #[test]
    fn rejects_legacy_allowlist_flags() {
        let legacy_flags = [
            ["--", "root"].concat(),
            ["--default", "-root", "-cwd"].concat(),
            ["--extra", "-rwx"].concat(),
        ];

        for flag in legacy_flags {
            let args = vec!["harnx-bash-tools".to_string(), flag.clone()];
            let mut inputs = AllowInputs::default();
            let mut config = initial_sandbox_config();
            let mut cli_files = Vec::new();
            let mut cli_dirs = Vec::new();
            let error = parse_cli_args(
                &args,
                &mut inputs,
                &mut config,
                &mut cli_files,
                &mut cli_dirs,
            )
            .expect_err("legacy flag should be rejected");
            assert!(error.contains(&format!("unknown argument: {flag}")));
        }
    }
}

//! `harnx-sandbox-run`: A standalone utility to run commands inside a birdcage sandbox with hook support.
//!
//! This binary provides a way to run arbitrary commands with the same sandboxing defaults and
//! credential injection hooks used by `harnx-bash-tools`. It is useful for testing hooks,
//! running tools securely, or as a building block for other agentic workflows.
//!
//! Sandboxing is delegated to `harnx-sandbox-exec`, which runs as a subprocess. This allows
//! the parent process to keep its tokio runtime alive — and with it, any sidecar hook processes
//! such as `harnx-proxy-auth` — for the full lifetime of the sandboxed command.
//!
//! ## Environment variables
//!
//! In addition to CLI flags, the following environment variables are supported (matching
//! `harnx-bash-tools`):
//!
//! | Variable | Format | Effect |
//! |---|---|---|
//! | `HARNX_TOOLS_ALLOW_READ` | Colon-separated paths | Allowed sandbox read-only paths |
//! | `HARNX_TOOLS_ALLOW_EXEC` | Colon-separated paths | Allowed sandbox execute paths |
//! | `HARNX_TOOLS_ALLOW_WRITE` | Colon-separated paths | Allowed sandbox writable paths |
//! | `HARNX_TOOLS_ALLOW_RWX` | Colon-separated paths | Allowed sandbox read/write/exec paths |
//! | `HARNX_BASH_ENV_PASSTHROUGH` | Comma-separated names | Extra host env var names to pass through |

mod cli;
mod hooks;
mod sandbox;

#[cfg(unix)]
use std::path::PathBuf;

use anyhow::Result;

/// Read a colon-separated path list from an env var.
#[cfg(unix)]
fn env_paths(var: &str) -> Vec<PathBuf> {
    std::env::var_os(var)
        .map(|val| {
            std::env::split_paths(&val)
                .filter(|p| !p.as_os_str().is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Read a comma-separated list of env var names from `HARNX_BASH_ENV_PASSTHROUGH`.
fn env_passthrough_names() -> Vec<String> {
    std::env::var("HARNX_BASH_ENV_PASSTHROUGH")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

fn merge_environment(cli: &mut cli::Cli) {
    #[cfg(unix)]
    {
        cli.allow_read.extend(env_paths("HARNX_TOOLS_ALLOW_READ"));
        cli.allow_write.extend(env_paths("HARNX_TOOLS_ALLOW_WRITE"));
        cli.allow_exec.extend(env_paths("HARNX_TOOLS_ALLOW_EXEC"));
        cli.allow_rwx.extend(env_paths("HARNX_TOOLS_ALLOW_RWX"));
    }

    cli.env_vars
        .extend(env_passthrough_names().into_iter().filter_map(|name| {
            std::env::var(&name)
                .ok()
                .map(|value| format!("{name}={value}"))
        }));
}

fn parse_cli(args: &[String]) -> cli::Cli {
    match <cli::Cli as clap::Parser>::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) => {
            let exit_code = match error.kind() {
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => 0,
                _ => 1,
            };
            let _ = error.print();
            std::process::exit(exit_code);
        }
    }
}

fn main() -> Result<()> {
    let _ = harnx_core::logging::init(harnx_core::logging::LogSink::Stderr);

    // Raw args (skip program name)
    let raw: Vec<String> = std::env::args().skip(1).collect();

    // Pre-parse --hook groups before clap
    let (hook_defs, remaining) = cli::pre_parse_hooks(raw)?;

    // Re-add program name for clap parsing
    let mut clap_args = vec!["harnx-sandbox-run".to_string()];
    clap_args.extend(remaining);

    // Treat stale configuration as a startup failure with the same exit code as
    // the tool servers. Help and version requests still exit successfully.
    let mut cli = parse_cli(&clap_args);

    // Merge env var overrides (matching harnx-bash-tools behaviour).
    merge_environment(&mut cli);

    // Run hooks if any, keeping the tokio runtime (and all sidecar processes,
    // e.g. harnx-proxy-auth) alive for the full duration of the child.
    let (hook_env, _rt, _manager) = if !hook_defs.is_empty() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let cwd = std::env::current_dir()?;
        let session_id = uuid::Uuid::new_v4().to_string();
        let result = rt.block_on(hooks::run_hooks(
            &hook_defs,
            &session_id,
            &cwd,
            &cli.command,
        ))?;
        // Keep rt and manager alive (bound to these variables) until after
        // setup_and_spawn returns — this keeps harnx-proxy-auth running.
        (result.env, Some(rt), Some(result.manager))
    } else {
        (std::collections::HashMap::new(), None, None)
    };

    // Delegate sandbox setup and spawn to harnx-sandbox-exec subprocess.
    // The tokio runtime and hook manager stay alive (via _rt and _manager above)
    // until this returns.
    let use_defaults = !cli.no_defaults;
    let exit_code = sandbox::setup_and_spawn(&cli, hook_env, use_defaults)?;

    // _rt and _manager drop here, shutting down harnx-proxy-auth after child exits.

    std::process::exit(exit_code);
}

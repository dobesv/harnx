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
//! | `HARNX_BASH_EXTRA_READABLE` | Colon-separated paths | Extra sandbox read-only paths |
//! | `HARNX_BASH_EXTRA_EXEC` | Colon-separated paths | Extra sandbox execute paths |
//! | `HARNX_BASH_EXTRA_WRITABLE` | Colon-separated paths | Extra sandbox writable paths |
//! | `HARNX_BASH_EXTRA_RWX` | Colon-separated paths | Extra sandbox read/write/exec paths |
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

fn main() -> Result<()> {
    env_logger::init();

    // Raw args (skip program name)
    let raw: Vec<String> = std::env::args().skip(1).collect();

    // Pre-parse --hook groups before clap
    let (hook_defs, remaining) = cli::pre_parse_hooks(raw)?;

    // Re-add program name for clap parsing
    let mut clap_args = vec!["harnx-sandbox-run".to_string()];
    clap_args.extend(remaining);

    // Parse remaining args with clap
    let mut cli = <cli::Cli as clap::Parser>::parse_from(&clap_args);

    // Merge env var overrides (matching harnx-bash-tools behaviour).
    #[cfg(unix)]
    {
        cli.extra_read
            .extend(env_paths("HARNX_BASH_EXTRA_READABLE"));
        cli.extra_write
            .extend(env_paths("HARNX_BASH_EXTRA_WRITABLE"));
        cli.extra_exec.extend(env_paths("HARNX_BASH_EXTRA_EXEC"));
        cli.extra_rwx.extend(env_paths("HARNX_BASH_EXTRA_RWX"));
    }

    // Extra env var names to pass through from host.
    for name in env_passthrough_names() {
        if let Ok(value) = std::env::var(&name) {
            cli.env_vars.push(format!("{name}={value}"));
        }
    }

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

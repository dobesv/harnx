//! Process supervision for hook server children.
//!
//! Spawns each hook server (mirroring TLS/env config from
//! `HookServerStartConfig` into the child) and monitors it until exit, then
//! hands off to `hook_crash` to install a fail-closed route for whatever the
//! crashed server was covering.

use super::hook_crash::{crash_marker, replace_crashed_hook_route};
use super::hook_supervisor::HookServerStartConfig;
use crate::config::{HARNX_NATS_REPLICAS_ENV, HARNX_NATS_TOKEN_ENV, HARNX_NATS_URL_ENV};
use anyhow::{bail, Context, Result};
use harnx_core::hooks::HookConfig;
use harnx_core::instance::{ServerScope, HARNX_SERVER_SCOPE};
use harnx_hooks::executor::HARNX_PACKAGE_DIR_ENV;
use harnx_hookset::{HookRegistration, HARNX_HOOK_NAME};
use harnx_nats_common::connect::{
    HARNX_NATS_TLS_CA_ENV, HARNX_NATS_TLS_CERT_ENV, HARNX_NATS_TLS_ENV, HARNX_NATS_TLS_KEY_ENV,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

pub(super) struct HookMonitor {
    pub(super) child: Child,
    pub(super) pid: u32,
    pub(super) server: String,
    pub(super) display_label: String,
    pub(super) registration: HookRegistration,
    pub(super) instance_id: ServerScope,
    pub(super) client: async_nats::Client,
    pub(super) processes: Arc<Mutex<HashMap<u32, String>>>,
}

/// Mirror this config's TLS/mTLS settings into the child's environment, using
/// the exact same variable names `NatsEndpoint::from_env` reads. A spawned
/// hook server that can't see these connects plaintext to a TLS-only broker
/// and never reaches it.
pub(super) fn apply_tls_env(command: &mut Command, config: &HookServerStartConfig) {
    if let Some(tls) = config.tls {
        command.env(HARNX_NATS_TLS_ENV, if tls { "true" } else { "false" });
    }
    if let Some(cert) = &config.tls_cert {
        command.env(HARNX_NATS_TLS_CERT_ENV, cert);
    }
    if let Some(key) = &config.tls_key {
        command.env(HARNX_NATS_TLS_KEY_ENV, key);
    }
    if let Some(ca) = &config.tls_ca {
        command.env(HARNX_NATS_TLS_CA_ENV, ca);
    }
}

pub(super) async fn spawn_hook_server(
    config: &HookServerStartConfig,
    hook: &HookConfig,
    name: &str,
) -> Result<Child> {
    let package_dir = hook
        .package_dir
        .clone()
        .unwrap_or_else(harnx_core::config_paths::config_dir);
    let mut words = shell_words::split(&hook.command).context("parse hook command")?;
    if words.is_empty() {
        bail!("hook command is empty");
    }
    let package_dir_value = package_dir.to_string_lossy();
    for word in &mut words[1..] {
        *word = word
            .replace("${HARNX_PACKAGE_DIR}", &package_dir_value)
            .replace("$HARNX_PACKAGE_DIR", &package_dir_value);
    }
    let binary = resolve_binary(&words[0])?;
    let mut command = Command::new(binary);
    command.args(&words[1..]);

    command
        .env(HARNX_PACKAGE_DIR_ENV, package_dir)
        .env(HARNX_HOOK_NAME, name)
        .env(HARNX_SERVER_SCOPE, config.instance_id.as_str())
        .env(HARNX_NATS_URL_ENV, &config.nats_url)
        .env(HARNX_NATS_TOKEN_ENV, &config.token)
        .env(
            HARNX_NATS_REPLICAS_ENV,
            config.resolved_replicas().to_string(),
        );
    apply_tls_env(&mut command, config);
    command
        .stdin(Stdio::null())
        // Send output where our own logs go so a hook server that exits before
        // registering explains itself instead of failing silently.
        .stdout(harnx_core::logging::child_output_sink())
        .stderr(harnx_core::logging::child_output_sink())
        .kill_on_drop(true);
    config
        .process_manager
        .spawn(command)
        .await
        .with_context(|| format!("spawn hook server '{name}'"))
}

fn resolve_binary(binary: &str) -> Result<PathBuf> {
    let current = std::env::current_exe().context("resolve current worker executable")?;
    let directory = current
        .parent()
        .context("current worker executable has no parent directory")?;
    let directory = if directory.file_name().is_some_and(|name| name == "deps") {
        directory
            .parent()
            .context("test executable deps directory has no parent")?
    } else {
        directory
    };
    let next_to_worker = directory.join(binary);
    #[cfg(windows)]
    let next_to_worker = next_to_worker.with_extension("exe");
    if next_to_worker.is_file() {
        return Ok(next_to_worker);
    }
    which::which(binary).with_context(|| {
        format!(
            "hook-server command '{binary}' not found next to worker at {} or on PATH",
            next_to_worker.display()
        )
    })
}

pub(super) fn spawn_child_monitor(monitor: HookMonitor) -> JoinHandle<()> {
    tokio::spawn(async move {
        let HookMonitor {
            mut child,
            pid,
            server,
            display_label,
            registration,
            instance_id,
            client,
            processes,
        } = monitor;
        let status = child.wait().await;
        processes.lock().await.remove(&pid);
        replace_crashed_hook_route(
            &client,
            &instance_id,
            &server,
            crash_marker(registration, display_label),
        )
        .await;
        log_child_exit(&server, status);
    })
}

fn log_child_exit(server: &str, status: std::io::Result<std::process::ExitStatus>) {
    match status {
        Ok(status) if status.success() => log::debug!("hook server '{server}' exited"),
        Ok(status) => log::warn!("hook server '{server}' exited with {status}"),
        Err(error) => log::warn!("hook server '{server}' wait failed: {error}"),
    }
}

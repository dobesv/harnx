//! Ask each tool server for its tools: run its command with `--list-tools
//! --format json` in front of its configured args, the environment the worker
//! gives it minus the broker, and read the listing from stdout.

use std::collections::HashMap;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use futures_util::future::join_all;
use harnx_core::child_process::ChildProcessManager;
use harnx_core::instance::HARNX_SERVER_SCOPE;
use harnx_toolset::listing::{ToolListing, LIST_FORMAT_FLAG, LIST_TOOLS_FLAG};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};

use crate::config::ToolServerConfig;
use crate::nats_worker::tool_supervisor::{apply_server_env, resolve_tool_binary};

/// Prefix of the broker variables a listing must not see, so a server never
/// connects anywhere while it lists.
const NATS_ENV_PREFIX: &str = "HARNX_NATS_";

/// The name each native tool server binary registers under when its args
/// give no `--name`, as its toolset's `name()` returns it.
pub const KNOWN_TOOLSET_NAMES: &[(&str, &str)] = &[
    ("harnx-attachment-tools", "attachments"),
    ("harnx-aws-creds", "aws_creds"),
    ("harnx-bash-tools", "bash"),
    ("harnx-claude-hook-tools", "claude_hook"),
    ("harnx-exa-tools", "exa"),
    ("harnx-fetch-tools", "fetch"),
    ("harnx-fs-tools", "fs"),
    ("harnx-grep-tools", "grep"),
    ("harnx-k8s-creds", "k8s_creds"),
    ("harnx-plans-tools", "plans"),
    ("harnx-proxy-auth", "proxy_auth"),
    ("harnx-time-tools", "time"),
];

/// The toolsets `harnx-k8s-sandbox-tools` registers: the flag and the
/// variable that rename each, and its default name.
const K8S_SANDBOX_TOOLSETS: [(&str, &str, &str); 3] = [
    ("--bash-name", "BASH_TOOLSET_NAME", "bash"),
    ("--fs-name", "FS_TOOLSET_NAME", "fs"),
    ("--sandbox-name", "SANDBOX_TOOLSET_NAME", "sandbox"),
];

/// What one tool server said about its tools.
#[derive(Debug, Clone)]
pub enum ServerListing {
    Listed(ToolListing),
    /// The tools couldn't be listed; `reason` says why, for the finding.
    Unverifiable {
        reason: String,
    },
}

/// The key [`list_servers`] files a server's listing under: its config file
/// stem, qualified by its package.
pub fn server_key(server: &ToolServerConfig) -> String {
    match &server.package {
        Some(package) => format!("{package}/{}", server.name),
        None => server.name.clone(),
    }
}

/// List every enabled server in `servers` at once, each within `timeout`.
pub async fn list_servers(
    servers: &[ToolServerConfig],
    timeout: Duration,
) -> HashMap<String, ServerListing> {
    let manager = ChildProcessManager::new();
    let listings = servers
        .iter()
        .filter(|server| server.enabled)
        .map(|server| async {
            (
                server_key(server),
                list_server(&manager, server, timeout).await,
            )
        });
    join_all(listings).await.into_iter().collect()
}

async fn list_server(
    manager: &ChildProcessManager,
    server: &ToolServerConfig,
    timeout: Duration,
) -> ServerListing {
    if let Some(reason) = needs_more_than_ci_has(&server.command) {
        return unverifiable(reason);
    }
    let binary = match resolve_tool_binary(server) {
        Ok(binary) => binary,
        Err(error) => return unverifiable(format!("{error:#}")),
    };
    let mut command = Command::new(&binary);
    command
        .arg(LIST_TOOLS_FLAG)
        .args([LIST_FORMAT_FLAG, "json"])
        .args(&server.args);
    apply_server_env(&mut command, server);
    without_broker_env(&mut command, server);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    match manager.spawn(command).await {
        Ok(child) => run_listing(manager, child, timeout).await,
        Err(error) => unverifiable(format!("failed to start {}: {error}", binary.display())),
    }
}

/// Servers that can't list their tools from a CI checkout, whatever their
/// args: they need a person's credentials or a live cluster.
fn needs_more_than_ci_has(command: &str) -> Option<&'static str> {
    match Path::new(command).file_stem()?.to_str()? {
        "harnx-mcp-remote" => {
            Some("harnx-mcp-remote needs user credentials to list the remote server's tools")
        }
        "harnx-k8s-sandbox-tools" => {
            Some("harnx-k8s-sandbox-tools needs a Kubernetes cluster to list its tools")
        }
        _ => None,
    }
}

/// The names a server registers under, read from its configuration without
/// running it, for when it can't be listed. A known native server registers
/// under its `--name`, else its toolset's default name
/// ([`KNOWN_TOOLSET_NAMES`]). A bridge or `harnx-mcp-remote` registers under
/// its `--name` (the bridge's own flags end at `--`), and `harnx-mcp-remote`
/// also reads `HARNX_MCP_REMOTE_NAME`. `harnx-k8s-sandbox-tools` registers
/// three toolsets, each named by its own flag or variable. Empty for any
/// other command, whose names only its listing can tell.
pub fn configured_names(server: &ToolServerConfig) -> Vec<String> {
    let Some(command) = Path::new(&server.command)
        .file_stem()
        .and_then(|stem| stem.to_str())
    else {
        return Vec::new();
    };
    let from_env = |key: &str| server.env.get(key).cloned();
    let named = |flag: &str, key: &str, default: &str| {
        flag_value(&server.args, flag)
            .or_else(|| from_env(key))
            .unwrap_or_else(|| default.to_string())
    };
    match command {
        "harnx-mcp-bridge" => flag_value(&server.args, "--name").into_iter().collect(),
        "harnx-mcp-remote" => flag_value(&server.args, "--name")
            .or_else(|| from_env("HARNX_MCP_REMOTE_NAME"))
            .into_iter()
            .collect(),
        "harnx-k8s-sandbox-tools" => K8S_SANDBOX_TOOLSETS
            .iter()
            .map(|(flag, key, default)| named(flag, key, default))
            .collect(),
        _ => KNOWN_TOOLSET_NAMES
            .iter()
            .find(|(binary, _)| *binary == command)
            .map(|(_, default)| flag_value(&server.args, "--name").unwrap_or(default.to_string()))
            .into_iter()
            .collect(),
    }
}

/// The value of `flag` in `args`, as `--flag VALUE` or `--flag=VALUE`, before
/// any `--`.
fn flag_value(args: &[String], flag: &str) -> Option<String> {
    let mut args = args.iter().take_while(|arg| *arg != "--");
    let prefix = format!("{flag}=");
    while let Some(arg) = args.next() {
        if arg == flag {
            return args.next().cloned();
        }
        if let Some(value) = arg.strip_prefix(&prefix) {
            return Some(value.to_string());
        }
    }
    None
}

fn unverifiable(reason: impl Into<String>) -> ServerListing {
    ServerListing::Unverifiable {
        reason: reason.into(),
    }
}

/// Drop every broker variable the server would otherwise inherit or be
/// configured with, and the registration scope only a worker hands out.
fn without_broker_env(command: &mut Command, server: &ToolServerConfig) {
    let inherited = std::env::vars_os().filter_map(|(key, _)| key.into_string().ok());
    let configured = server.env.keys().cloned();
    for key in inherited.chain(configured) {
        if key.starts_with(NATS_ENV_PREFIX) {
            command.env_remove(key);
        }
    }
    command.env_remove(HARNX_SERVER_SCOPE);
}

/// Read the listing and the server's exit status, all within one deadline.
///
/// Both pipes are read to their end before the server is reaped: a pipe
/// stays open while anything in the server's process group holds it, such
/// as a grandchild left running. Once both close, the group is killed
/// before the wait reaps the server, while its unreaped pid still reserves
/// the group ID, so nothing the server started outlives the listing. When
/// the deadline passes first, the group is killed the same way.
async fn run_listing(
    manager: &ChildProcessManager,
    mut child: Child,
    timeout: Duration,
) -> ServerListing {
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    let finished = tokio::time::timeout(timeout, async {
        let (stdout, stderr) = tokio::join!(read_pipe(stdout), read_pipe(stderr));
        let _ = manager.stop_process_group(&child);
        (child.wait().await, stdout, stderr)
    })
    .await;
    match finished {
        Ok((Ok(status), stdout, stderr)) => listing_outcome(status, &stdout, &stderr),
        Ok((Err(error), _, _)) => unverifiable(format!("failed waiting for it: {error}")),
        Err(_) => {
            let _ = manager.stop_process_group(&child);
            let _ = child.kill().await;
            unverifiable(format!(
                "timed out after {}s listing its tools",
                timeout.as_secs_f64()
            ))
        }
    }
}

async fn read_pipe(pipe: Option<impl AsyncRead + Unpin>) -> String {
    let mut bytes = Vec::new();
    if let Some(mut pipe) = pipe {
        let _ = pipe.read_to_end(&mut bytes).await;
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn listing_outcome(status: ExitStatus, stdout: &str, stderr: &str) -> ServerListing {
    if !status.success() {
        let exit = status.code().map_or_else(
            || status.to_string(),
            |code| format!("exited with code {code}"),
        );
        return unverifiable(format!("{exit} listing its tools{}", quoted_error(stderr)));
    }
    match serde_json::from_str::<ToolListing>(stdout) {
        Ok(listing) => ServerListing::Listed(listing),
        Err(error) => unverifiable(format!(
            "printed no tool listing ({error}){}",
            quoted_error(stderr)
        )),
    }
}

/// `: <error>` for the line of a server's stderr that says why it failed,
/// with any credentials in a URL hidden, or nothing when no line does.
///
/// Only that one line is quoted: the rest is log records, which can carry
/// secrets (the bridge logs the proxy URLs it inherits) and say nothing about
/// the failure, or a backtrace.
fn quoted_error(stderr: &str) -> String {
    match error_line(stderr) {
        Some(line) => format!(": {}", truncated(&redact_userinfo(line))),
        None => String::new(),
    }
}

/// The last line starting with `Error:` (as a Rust `main` returning an error
/// prints it), without that prefix; else the last line that's neither a log
/// record nor part of a backtrace.
fn error_line(stderr: &str) -> Option<&str> {
    let lines = || {
        stderr
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
    };
    let error = lines().rev().find_map(|line| {
        let (prefix, rest) = line.split_at_checked(ERROR_PREFIX.len())?;
        prefix
            .eq_ignore_ascii_case(ERROR_PREFIX)
            .then(|| rest.trim_start())
    });
    error.or_else(|| {
        lines()
            .rev()
            .find(|line| !is_log_record(line) && !is_backtrace_line(line))
    })
}

const ERROR_PREFIX: &str = "error:";

/// The most of a quoted line a finding repeats.
const MAX_QUOTED_CHARS: usize = 300;

fn truncated(line: &str) -> String {
    match line.char_indices().nth(MAX_QUOTED_CHARS) {
        Some((end, _)) => format!("{}…", &line[..end]),
        None => line.to_string(),
    }
}

/// The digits of a log record's year, before the `-` that follows them.
const YEAR_DIGITS: usize = 4;

/// A line `harnx_core::logging` writes: `2026-08-16T12:34:56.789Z [INFO ]
/// ...` in text, or a JSON object with a `level` in JSON.
fn is_log_record(line: &str) -> bool {
    let starts_with_timestamp = line.split_whitespace().next().is_some_and(|first| {
        let bytes = first.as_bytes();
        bytes.len() > YEAR_DIGITS
            && bytes[..YEAR_DIGITS].iter().all(u8::is_ascii_digit)
            && bytes[YEAR_DIGITS] == b'-'
            && first.contains('T')
    });
    starts_with_timestamp || (line.starts_with('{') && line.contains("\"level\""))
}

/// A line of a Rust backtrace: its header, a numbered frame, a frame's
/// `at file:line`, or the note after it.
fn is_backtrace_line(line: &str) -> bool {
    let numbered_frame = line.split_once(": ").is_some_and(|(number, _)| {
        !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())
    });
    numbered_frame
        || line.starts_with("at ")
        || line == "stack backtrace:"
        || (line.starts_with("note: ") && line.contains("RUST_BACKTRACE"))
}

const SCHEME_SEPARATOR: &str = "://";

/// `line` with the user and password of every URL replaced by `***`:
/// `http://user:pass@host` becomes `http://***@host`.
fn redact_userinfo(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(start) = rest.find(SCHEME_SEPARATOR) {
        let (before, after) = rest.split_at(start + SCHEME_SEPARATOR.len());
        out.push_str(before);
        let authority_end = after
            .find(|c: char| {
                matches!(c, '/' | '?' | '#' | '"' | '\'' | '<' | '>') || c.is_whitespace()
            })
            .unwrap_or(after.len());
        match after[..authority_end].rfind('@') {
            Some(at) => {
                out.push_str("***");
                rest = &after[at..];
            }
            None => rest = after,
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_userinfo_in_every_url() {
        assert_eq!(
            redact_userinfo("a http://u:p@h:1/x and https://tok@h2 ok"),
            "a http://***@h:1/x and https://***@h2 ok"
        );
        assert_eq!(redact_userinfo("http://host/a@b"), "http://host/a@b");
        assert_eq!(redact_userinfo("no url: a@b"), "no url: a@b");
    }

    #[test]
    fn error_line_prefers_an_error_prefix() {
        let stderr =
            "Error: first\n2026-11-25T10:00:00.000Z [INFO ] 1 x: log\nERROR: last\ntrailing";
        assert_eq!(error_line(stderr), Some("last"));
    }

    #[test]
    fn error_line_skips_logs_and_backtraces() {
        let stderr = "thread 'main' panicked at src/main.rs:2:5:\nboom\nstack backtrace:\n   0: std::rt\n             at /x.rs:1:1\nnote: run with `RUST_BACKTRACE=1` for a backtrace\n{\"ts\":\"t\",\"level\":\"INFO\"}\n";
        assert_eq!(error_line(stderr), Some("boom"));
        assert_eq!(error_line("   0: std::rt\n"), None);
    }

    #[test]
    fn long_lines_are_truncated() {
        let line = "é".repeat(MAX_QUOTED_CHARS + 5);
        assert_eq!(truncated(&line).chars().count(), MAX_QUOTED_CHARS + 1);
    }
}

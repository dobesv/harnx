//! Credential management commands shared by the CLI and the TUI (decision 11).
//!
//! These commands act for the *operator's* identity, not the active session's
//! owner: the caller-supplied transport fills `OperatorToolRequest.user_id`
//! from the cluster default. Secrets are never accepted here — connect flows
//! hand the user a link whose secret exchange happens on the tool server's
//! own page.

mod connect;

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::future::Future;

use super::{OperatorToolCommand, OperatorToolInput, OperatorToolReply};
use crate::utils::AbortSignal;

/// Declaration glob that finds every credentialed server's status tool.
const STATUS_TOOL_PATTERN: &str = "*_credential_status";
const STATUS_TOOL_SUFFIX: &str = "_credential_status";

/// One credential management action requested by the CLI or TUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialsAction {
    /// `harnx credentials list` / `.credentials list`
    List,
    /// `harnx credentials connect <server>` / `.credentials connect <server>`
    Connect { server: String },
    /// `harnx credentials disconnect <server>` / `.credentials disconnect <server>`
    Disconnect { server: String },
}

/// Parse the remainder of a `.credentials` line (`list`, `connect <server>`,
/// `disconnect <server>`; a bare `.credentials` means `list`).
pub fn parse_credentials_action(rest: &str) -> Result<CredentialsAction> {
    let mut words = rest.split_whitespace();
    match words.next() {
        None | Some("list") => Ok(CredentialsAction::List),
        Some("connect") => {
            let Some(server) = words.next() else {
                bail!("Usage: .credentials connect <server>")
            };
            Ok(CredentialsAction::Connect {
                server: server.to_string(),
            })
        }
        Some("disconnect") => {
            let Some(server) = words.next() else {
                bail!("Usage: .credentials disconnect <server>")
            };
            Ok(CredentialsAction::Disconnect {
                server: server.to_string(),
            })
        }
        Some(other) => bail!("unknown credentials command '{other}'; use list, connect <server> or disconnect <server>"),
    }
}

/// Mid-run progress the frontend should show the user. Only connect flows
/// report progress: the link to open in a browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialsProgress {
    /// A connect link to open in a browser. Reported when the link changes,
    /// not on every poll.
    ConnectUrl { url: String, message: String },
}

/// Final outcome of one credential command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialsOutcome {
    /// Human-readable result text.
    pub output: String,
    /// Number of per-server calls that failed (list only). Nonzero means the
    /// caller should exit nonzero while still showing `output`.
    pub failures: usize,
}

/// Run one credential management command against the caller's operator tool
/// transport.
///
/// `call` dispatches a single [`OperatorToolCommand`] with JSON output
/// (`json = true`); the CLI routes it through its reserved/named operator
/// machinery and the TUI through the active session's operator request.
/// Every command the engine builds carries only non-secret arguments:
/// `{}` for the management tools, and for connect polling the transport-owned
/// `tool_input` with the accepted `url`-challenge response. `abort` is the
/// user's cancel signal (Ctrl-C): a running connect flow answers it by
/// sending the `cancel` action once, best effort.
pub async fn run_credentials_command<F, Fut>(
    action: CredentialsAction,
    abort: &AbortSignal,
    mut report: impl FnMut(CredentialsProgress),
    mut call: F,
) -> Result<CredentialsOutcome>
where
    F: FnMut(OperatorToolCommand) -> Fut,
    Fut: Future<Output = Result<OperatorToolReply>>,
{
    match action {
        CredentialsAction::List => run_list(&mut call).await,
        CredentialsAction::Connect { server } => {
            connect::run_connect(server, abort, &mut report, &mut call).await
        }
        CredentialsAction::Disconnect { server } => {
            let name = format!("{server}_disconnect");
            let reply = call(management_call(name, None)).await?;
            reply_error(&reply)?;
            Ok(CredentialsOutcome {
                output: result_text(&reply),
                failures: 0,
            })
        }
    }
}

async fn run_list<F, Fut>(call: &mut F) -> Result<CredentialsOutcome>
where
    F: FnMut(OperatorToolCommand) -> Fut,
    Fut: Future<Output = Result<OperatorToolReply>>,
{
    let reply = call(OperatorToolCommand::List {
        pattern: Some(STATUS_TOOL_PATTERN.to_string()),
    })
    .await?;
    reply_error(&reply)?;
    let declarations = parse_output(&reply.output)?;
    let mut servers: Vec<String> = declarations
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|declaration| declaration.get("name").and_then(Value::as_str))
        .filter(|name| name.ends_with(STATUS_TOOL_SUFFIX))
        .filter_map(|name| name.strip_suffix(STATUS_TOOL_SUFFIX))
        .map(str::to_string)
        .collect();
    servers.sort();
    servers.dedup();
    if servers.is_empty() {
        return Ok(CredentialsOutcome {
            output: "No credentialed tool servers are available on this cluster.".to_string(),
            failures: 0,
        });
    }
    let mut lines = Vec::new();
    let mut failures = 0usize;
    for server in &servers {
        let reply = call(management_call(format!("{server}_credential_status"), None)).await;
        match reply {
            Ok(reply) if reply.error.is_none() => {
                lines.push(status_line(server, &parse_output(&reply.output)?));
            }
            Ok(reply) => {
                failures += 1;
                lines.push(format!(
                    "{server}: error: {}",
                    reply.error.unwrap_or_else(|| "tool failed".into())
                ));
            }
            Err(error) => {
                failures += 1;
                lines.push(format!("{server}: error: {error:#}"));
            }
        }
    }
    Ok(CredentialsOutcome {
        output: lines.join("\n"),
        failures,
    })
}

/// One `harnx credentials list` line for a server's `credential_status`
/// result. `credential_status` returns non-secret fields only (connected,
/// account label, absolute expiry, active/needs-login status).
fn status_line(server: &str, status: &Value) -> String {
    let connected = status.get("connected").and_then(Value::as_bool);
    if connected != Some(true) {
        return format!("{server}: not connected");
    }
    let label = status.get("account_label").and_then(Value::as_str);
    let expires = status.get("expires_at").and_then(Value::as_str);
    let expires = expires.map(|value| format!("expires {value}"));
    let needs_login = status.get("status").and_then(Value::as_str) == Some("needs_login");
    let mut parts: Vec<String> = Vec::new();
    let state = if needs_login {
        "connected but needs re-login"
    } else {
        "connected"
    };
    parts.push(state.to_string());
    if let Some(label) = label {
        parts.push(format!("as {label}"));
    }
    if let Some(expires) = expires {
        parts.push(expires);
    }
    format!("{server}: {}", parts.join(", "))
}

fn management_call(name: String, tool_input: Option<OperatorToolInput>) -> OperatorToolCommand {
    OperatorToolCommand::Call {
        name,
        args_json: "{}".to_string(),
        tool_input,
    }
}

fn reply_error(reply: &OperatorToolReply) -> Result<()> {
    match &reply.error {
        Some(error) => bail!("{error}"),
        None => Ok(()),
    }
}

/// Parse the JSON tool result the transport returned (`json = true` output).
fn parse_output(output: &str) -> Result<Value> {
    serde_json::from_str(output).with_context(|| format!("invalid tool result: {output}"))
}

/// Human text from a tool result: MCP-style content text when present, the
/// raw JSON otherwise.
fn result_text(reply: &OperatorToolReply) -> String {
    let Ok(value) = serde_json::from_str::<Value>(&reply.output) else {
        return reply.output.clone();
    };
    let text = value
        .get("content")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    (item.get("type").and_then(Value::as_str) == Some("text"))
                        .then(|| item.get("text").and_then(Value::as_str).map(str::to_string))
                        .flatten()
                })
                .collect::<Vec<_>>()
        })
        .filter(|parts| !parts.is_empty())
        .map(|parts| parts.join("\n"));
    match text {
        Some(text) => text,
        None if reply.output.trim().is_empty() => String::new(),
        None => reply.output.trim().to_string(),
    }
}

#[cfg(test)]
mod tests;

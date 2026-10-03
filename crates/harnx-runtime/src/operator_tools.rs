//! Explicit operator tools, without a model request or shared approval policy.

pub mod cli;

use crate::config::{GlobalConfig, LOCAL_CLUSTER_KEY};
use crate::nats_worker::{LocalWorkerTarget, SessionActivationRoute};
use crate::tool_output::{
    format_tool_declaration, format_tool_list, format_tool_result, ToolOutputFormat,
};
use anyhow::{bail, ensure, Context, Result};
use futures_util::StreamExt;
use harnx_core::tool::{ToolCall, ToolDeclaration};
use harnx_engine::tool::ToolEvalContext;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum OperatorToolCommand {
    Info { name: String },
    List { pattern: Option<String> },
    Call { name: String, args_json: String },
}

/// Split only the grammar prefix. JSON is a raw remainder, not shell words.
pub fn parse_operator_tool_command(cmd: &str, args: Option<&str>) -> Result<OperatorToolCommand> {
    let args = args.unwrap_or_default();
    let (subject, rest) = split_word(args);
    match (cmd, subject) {
        (".info", "tool") if !rest.is_empty() && rest.split_whitespace().count() == 1 => {
            Ok(OperatorToolCommand::Info {
                name: rest.trim_end().into(),
            })
        }
        (".list", "tools") if rest.split_whitespace().count() <= 1 => {
            Ok(OperatorToolCommand::List {
                pattern: (!rest.is_empty()).then(|| rest.trim_end().into()),
            })
        }
        (".call", "tool") => {
            let (name, raw) = split_word(rest);
            ensure!(
                !name.is_empty() && !raw.is_empty(),
                "Usage: .call tool <name> <args-json>"
            );
            parse_arguments(raw)?;
            Ok(OperatorToolCommand::Call {
                name: name.into(),
                args_json: raw.into(),
            })
        }
        _ => {
            bail!("Usage: .info tool <name>; .list tools [pattern]; .call tool <name> <args-json>")
        }
    }
}

pub fn parse_operator_line(line: &str) -> Result<OperatorToolCommand> {
    let (cmd, args) = split_word(line);
    parse_operator_tool_command(cmd, Some(args))
}

fn split_word(text: &str) -> (&str, &str) {
    let text = text.trim_start();
    let end = text.find(char::is_whitespace).unwrap_or(text.len());
    (&text[..end], text[end..].trim_start())
}

pub fn is_operator_tool_command(line: &str) -> bool {
    let (cmd, args) = split_word(line);
    let subject = args.split_whitespace().next();
    matches!(
        (cmd, subject),
        (".info", Some("tool")) | (".list", _) | (".call", _)
    )
}

pub(crate) fn parse_arguments(raw: &str) -> Result<serde_json::Value> {
    let value: serde_json::Value =
        serde_json::from_str(raw).context("Invalid tool argument JSON")?;
    ensure!(value.is_object(), "Tool arguments must be a JSON object");
    Ok(value)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct OperatorToolRequest {
    pub version: u32,
    pub session_key: String,
    pub use_tools: Option<Vec<String>>,
    pub tool_use: bool,
    pub command: OperatorToolCommand,
    pub json: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum OperatorToolMessage {
    Started,
    Finished(OperatorToolReply),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OperatorToolReply {
    pub output: String,
    pub error: Option<String>,
}

pub(crate) fn subject(cluster: &str, route: &SessionActivationRoute) -> Result<String> {
    Ok(match route {
        SessionActivationRoute::ClusterShared => format!("cluster.{cluster}.operator_tools"),
        SessionActivationRoute::WorkerTargeted {
            session_scope,
            worker_id,
        } => {
            let target = LocalWorkerTarget::new(session_scope, worker_id)?;
            format!(
                "session_scope.{}.workers.{}.operator_tools",
                target.session_scope(),
                target.worker_id()
            )
        }
    })
}

/// Capture the active session before awaiting routing. Never creates a default
/// agent or virtual caller session. All provider/hook execution stays on workers.
pub async fn run_session_tool_command(
    config: &GlobalConfig,
    abort: &crate::utils::AbortSignal,
    command: OperatorToolCommand,
    json: bool,
    local_worker: &Arc<
        tokio::sync::Mutex<Option<crate::local_orchestrator::LocalWorkerSupervisor>>,
    >,
) -> Result<OperatorToolReply> {
    ensure!(!abort.aborted(), "Operator tool command cancelled");
    let snapshot = config.read().clone();
    ensure!(
        snapshot.session.is_some(),
        "Tool commands require an active session"
    );
    let captured = Arc::new(crate::config::ConfigLock::new(snapshot.clone()));
    let session = crate::config::remote_session_ops::remote_nats_session(&captured, abort).await?;
    let cluster = snapshot
        .remote_agent
        .as_ref()
        .map(|(_, cluster)| cluster.as_str())
        .unwrap_or_else(|| snapshot.default_cluster_key());
    let route = if cluster == LOCAL_CLUSTER_KEY {
        crate::local_orchestrator::activation_route_for_cluster(
            cluster,
            local_worker,
            abort.clone(),
        )
        .await?
    } else {
        SessionActivationRoute::ClusterShared
    };
    let request = OperatorToolRequest {
        version: 1,
        session_key: session.storage_key().to_owned(),
        use_tools: snapshot
            .session
            .as_ref()
            .and_then(|session| session.use_tools.clone())
            .or_else(|| snapshot.agent.as_ref().and_then(|agent| agent.use_tools())),
        tool_use: snapshot.tool_use,
        command,
        json,
    };
    request_command(
        &session.jetstream().client(),
        &subject(cluster, &route)?,
        &request,
        abort,
    )
    .await
}

pub(crate) async fn request_command(
    client: &async_nats::Client,
    subject: &str,
    request: &OperatorToolRequest,
    abort: &crate::utils::AbortSignal,
) -> Result<OperatorToolReply> {
    struct CancelOnDrop(crate::utils::AbortSignal);
    impl Drop for CancelOnDrop {
        fn drop(&mut self) {
            self.0.set_ctrlc();
        }
    }
    let own_abort = crate::utils::create_abort_signal();
    let _cancel = CancelOnDrop(own_abort.clone());
    let client = client.clone();
    let subject = subject.to_owned();
    let request = request.clone();
    let mut task = tokio::spawn(async move {
        request_command_inner(&client, &subject, &request, &own_abort).await
    });
    tokio::select! {
        result = &mut task => result.context("Operator request task failed")?,
        _ = crate::utils::wait_abort_signal(abort) => {
            _cancel.0.set_ctrlc();
            // The owned request task completes the acknowledgement/cancel handshake
            // even if the frontend retires this future or closes its completion popup.
            bail!("Operator tool command cancelled")
        }
    }
}

async fn request_command_inner(
    client: &async_nats::Client,
    subject: &str,
    request: &OperatorToolRequest,
    abort: &crate::utils::AbortSignal,
) -> Result<OperatorToolReply> {
    let inbox = client.new_inbox();
    let mut replies = client.subscribe(inbox.clone()).await?;
    client.flush().await?;
    ensure!(!abort.aborted(), "Operator tool command cancelled");
    client
        .publish_with_reply(
            subject.to_owned(),
            inbox.clone(),
            serde_json::to_vec(request)?.into(),
        )
        .await?;
    // Wait for subscription acknowledgement even on cancellation, so cancellation
    // cannot be lost before the worker installs its per-request listener.
    let first = tokio::time::timeout(Duration::from_secs(60), replies.next())
        .await
        .context("Operator tool worker did not acknowledge the request")?
        .context("Operator tool reply channel closed")?;
    match serde_json::from_slice::<OperatorToolMessage>(&first.payload)
        .context("Invalid operator tool reply")?
    {
        OperatorToolMessage::Finished(reply) => return Ok(reply),
        OperatorToolMessage::Started => {}
    }
    tokio::select! {
        biased;
        _ = crate::utils::wait_abort_signal(abort) => {
            client.publish(format!("{inbox}.cancel"), "".into()).await?;
            client.flush().await?;
            bail!("Operator tool command cancelled")
        }
        message = tokio::time::timeout(Duration::from_secs(660), replies.next()) => {
            let message = message.context("Operator tool worker disconnected or timed out; execution outcome may be unknown")?.context("Operator tool reply channel closed")?;
            match serde_json::from_slice::<OperatorToolMessage>(&message.payload)? {
                OperatorToolMessage::Finished(reply) => Ok(reply),
                OperatorToolMessage::Started => bail!("Duplicate operator tool acknowledgement"),
            }
        }
    }
}

/// Shared evaluation for session and reservation-owned operator commands.
pub(crate) async fn evaluate(
    ctx: ToolEvalContext,
    command: &OperatorToolCommand,
    format: ToolOutputFormat,
    abort: &crate::utils::AbortSignal,
) -> Result<OperatorToolReply> {
    ensure!(!abort.aborted(), "Operator tool command cancelled");
    let declarations = allowed_declarations(&ctx);
    let output = match command {
        OperatorToolCommand::Info { name } => {
            format_tool_declaration(find_tool(&declarations, name)?, format)?
        }
        OperatorToolCommand::List { pattern } => {
            let selector = pattern
                .as_deref()
                .map(|pattern| crate::tool_selector::ToolSelector::new(pattern, true));
            let selected: Vec<_> = declarations
                .into_iter()
                .filter(|tool| selector.as_ref().is_none_or(|s| s.is_match(&tool.name)))
                .collect();
            format_tool_list(&selected, format)?
        }
        OperatorToolCommand::Call { name, args_json } => {
            find_tool(&declarations, name)?;
            let call = ToolCall {
                name: name.clone(),
                arguments: parse_arguments(args_json)?,
                id: None,
                thought_signature: None,
                reasoning_provenance: None,
            };
            let result = harnx_engine::tool::eval_operator_tool_call(ctx, call, abort).await?;
            let error = if result.switch_agent.is_some() {
                Some("Session handoff requires an agent turn; handoff was not applied".into())
            } else {
                result_error(&result.output)
            };
            return Ok(OperatorToolReply {
                output: format_tool_result(&result.output, format)?,
                error,
            });
        }
    };
    Ok(OperatorToolReply {
        output,
        error: None,
    })
}

fn allowed_declarations(ctx: &ToolEvalContext) -> Vec<ToolDeclaration> {
    let mut tools: Vec<_> = ctx
        .render
        .as_ref()
        .map(|render| {
            render
                .decl_map
                .values()
                .filter(|tool| ctx.allowed_tool_names.contains(&tool.name))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    tools
}

fn find_tool<'a>(tools: &'a [ToolDeclaration], name: &str) -> Result<&'a ToolDeclaration> {
    tools
        .iter()
        .find(|tool| tool.name == name)
        .with_context(|| format!("Tool '{name}' is not available in the active session"))
}

fn result_error(output: &serde_json::Value) -> Option<String> {
    let failed = ["isError", "is_error", "blocked_by_hook"]
        .iter()
        .any(|key| output.get(key).and_then(serde_json::Value::as_bool) == Some(true));
    let partial = output.get("partial").and_then(serde_json::Value::as_bool) == Some(true)
        || output.get("resultType").and_then(serde_json::Value::as_str) == Some("partial");
    if partial {
        Some("Tool returned a partial result, not completed success".into())
    } else if failed {
        Some(
            output
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("Tool reported an error")
                .into(),
        )
    } else {
        None
    }
}

#[cfg(test)]
mod tests;

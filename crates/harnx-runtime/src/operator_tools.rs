//! Explicit operator tools, without a model request or shared approval policy.

pub mod cli;
pub mod credentials;

use crate::config::{GlobalConfig, LOCAL_CLUSTER_KEY};
use crate::nats_worker::{LocalWorkerTarget, SessionActivationRoute};
use crate::tool_output::{
    format_tool_declaration, format_tool_list, format_tool_result, ToolOutputFormat,
};
use anyhow::{bail, ensure, Context, Result};
use futures_util::StreamExt;
use harnx_core::tool::{ToolCall, ToolDeclaration};
use harnx_engine::tool::{eval_inner_tool_call, ToolEvalContext};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum OperatorToolCommand {
    Info {
        name: String,
    },
    List {
        pattern: Option<String>,
    },
    Call {
        name: String,
        args_json: String,
        /// Transport-owned rerun input for credential connect flows
        /// (decision 11): the accepted challenge response plus the tool's
        /// opaque request state, delivered beside the arguments exactly the
        /// way the durable model path delivers them — never inside the args.
        /// Absent on every pre-credentials request, so old wire forms are
        /// unchanged.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_input: Option<OperatorToolInput>,
    },
    /// Operator BashKit script call (#2630). `script` is the raw, unmodified
    /// script; `selection` records how its inner tools were chosen, kept
    /// separate so the worker can apply the claims/admission matrix without
    /// re-parsing the script.
    Bash {
        script: String,
        selection: BashToolSelection,
    },
}

/// Rerun input one operator `call` carries beside its arguments (decision 11):
/// the accepted response to the tool's latest input challenge, keyed by the
/// challenge key, plus the tool's opaque request state. The CLI/TUI fill it
/// when polling a `<server>_connect` flow; the runtime injects it into the
/// tool request the same way the durable model path does.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OperatorToolInput {
    pub responses: std::collections::BTreeMap<String, harnx_toolset::ToolInputResponse>,
    pub request_state: serde_json::Value,
}

impl OperatorToolInput {
    /// One `url`-challenge answer for a connect flow round. `url` responses
    /// carry nothing beyond the action (decision 6).
    pub fn url_response(
        challenge_key: String,
        action: &str,
        request_state: serde_json::Value,
    ) -> Self {
        Self {
            responses: std::collections::BTreeMap::from([(
                challenge_key,
                harnx_toolset::ToolInputResponse {
                    action: action.to_string(),
                    data: Default::default(),
                    note: None,
                },
            )]),
            request_state,
        }
    }
}

/// Tool selection intent for an operator Bash script call (#2630).
///
/// `Session` and `Inferred` distinguish "no explicit selectors given" from
/// `Explicit`, which stays meaningful even when its selector list resolves to
/// nothing: explicit presence always disables inference.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum BashToolSelection {
    /// CLI call without explicit selectors. `names` are exact literal command
    /// names from bounded static analysis — never globs or toolset selectors,
    /// so resolving them can only narrow, never widen, admission.
    Inferred { names: Vec<String> },
    /// CLI call with explicit selectors, resolved through the normal
    /// package/toolset/glob path. An empty vector is still explicit: the
    /// operator asked for a pure shell run.
    Explicit { selectors: Vec<String> },
    /// TUI `.call bash`: the worker resolves the effective session use_tools
    /// plus bash_tools. No per-call inference.
    Session,
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
        // `.info tools` is an alias of `.list tools` kept for muscle memory:
        // both spellings route to the worker, which owns the session's real
        // tool list — the frontend only sees builtins and handoff
        // declarations (#2628). It takes no arguments; a pattern belongs in
        // `.list tools [pattern]`.
        (".info", "tools") if rest.trim().is_empty() => {
            Ok(OperatorToolCommand::List { pattern: None })
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
                tool_input: None,
            })
        }
        // `.call bash` (#2630): the raw remainder is the script, preserving
        // its quoting and spacing. Selection is the effective session
        // use_tools plus bash_tools — no per-call inference or flags.
        (".call", "bash") => {
            ensure!(!rest.is_empty(), "Usage: .call bash <script>");
            Ok(OperatorToolCommand::Bash {
                script: rest.into(),
                selection: BashToolSelection::Session,
            })
        }
        _ => {
            bail!("Usage: .info tool <name>; .info tools; .list tools [pattern]; .call tool <name> <args-json>; .call bash <script>")
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
        (".info", Some("tool")) | (".info", Some("tools")) | (".list", _) | (".call", _)
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
    /// Positive requested script timeout for a Bash command (#2630),
    /// propagated to the script within the current operator bounds (the
    /// worker still wraps execution at its own deadline). `None` keeps the
    /// script backend's default. Absent on old requests and on every other
    /// command, so the serialized old forms are unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bash_timeout_secs: Option<u64>,
    /// The operator's harnx user identity (decision 11): the CLI/TUI fill it
    /// from `Config::default_user_id` for the selected cluster, and the
    /// worker forwards it into `ToolRequest.user_id` for these calls instead
    /// of the session owner. The outer option distinguishes a legacy request
    /// that predates the field (`None`: the worker keeps the session owner)
    /// from an explicit operator identity, including an explicit ownerless
    /// one (`Some(None)`).
    #[serde(
        default,
        with = "operator_user_id",
        skip_serializing_if = "Option::is_none"
    )]
    pub user_id: Option<Option<String>>,
}

/// Serde helpers for the tri-state [`OperatorToolRequest::user_id`]: the field
/// serializes as a plain optional, but an explicit `null` deserializes as
/// `Some(None)` (operator with no identity) while an absent field stays the
/// `default` `None` (legacy request).
mod operator_user_id {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub(crate) fn serialize<S>(
        value: &Option<Option<String>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        value
            .as_ref()
            .and_then(Option::as_ref)
            .serialize(serializer)
    }

    pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<Option<Option<String>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Some(Option::<String>::deserialize(deserializer)?))
    }
}

/// Operator Bash claims and admission selectors for worker-side commands
/// (#2630): the named-CLI and TUI rows of the selection matrix. Claims are
/// startup hints only; admission is authorization, and named inference never
/// widens agent permissions.
///
/// - Inferred: claims the literal names; admits `Resolve(U ∪ B)`.
/// - Explicit: claims and admit `Resolve(U ∪ B ∪ E)`.
/// - Session: claims and admit `Resolve(U ∪ B)`.
pub(crate) fn bash_selection_selectors(
    selection: &BashToolSelection,
    agent_use_tools: Option<Vec<String>>,
    agent_bash_tools: Option<Vec<String>>,
) -> (Vec<String>, Vec<String>) {
    let union = |explicit: Option<&[String]>| {
        let mut selectors = Vec::new();
        for part in [
            agent_use_tools.as_deref(),
            agent_bash_tools.as_deref(),
            explicit,
        ] {
            for selector in part.into_iter().flatten() {
                let selector = selector.trim();
                if !selector.is_empty() && !selectors.iter().any(|s| s == selector) {
                    selectors.push(selector.to_string());
                }
            }
        }
        selectors
    };
    match selection {
        BashToolSelection::Inferred { names } => {
            let admission = union(None);
            (names.clone(), admission)
        }
        BashToolSelection::Explicit { selectors } => {
            let union = union(Some(selectors));
            (union.clone(), union)
        }
        BashToolSelection::Session => {
            let union = union(None);
            (union.clone(), union)
        }
    }
}

/// Inner-tool declarations admitted for one operator Bash run: the
/// resolved admission names intersected with the discovered inventory, minus
/// control tools. The script catalog, the executor allowlist, and the
/// declaration map all derive from this single admitted set. The agentless
/// reserved path (CLI) uses this directly against its provider-only
/// inventory; the named/session worker path uses
/// [`operator_bash_inner_declarations`], which adds the in-process built-ins.
pub(crate) fn inner_declarations_for(
    resolved: &HashSet<String>,
    discovered: &[ToolDeclaration],
) -> Vec<ToolDeclaration> {
    discovered
        .iter()
        .filter(|declaration| {
            resolved.contains(&declaration.name)
                && crate::config::is_bash_inner_tool_allowed(&declaration.name)
        })
        .cloned()
        .collect()
}

/// Inner-tool declarations admitted for a named/session operator Bash run
/// (#2655 finding B): the resolved admission names mapped onto the same
/// declaration universe ordinary agent Bash dispatches through — the
/// discovered NATS inventory plus the built-ins the evaluation context's
/// in-process providers answer (session history and session metadata).
/// A discovered declaration wins over a built-in of the same name, matching
/// both provider order (NATS before the built-in providers) and the context
/// declaration map. Names backed by neither stay out; control tools are
/// dropped. The script catalog, the executor allowlist, and the allowed
/// names all derive from this single admitted set.
pub(crate) fn operator_bash_inner_declarations(
    resolved: &HashSet<String>,
    discovered: &[ToolDeclaration],
) -> Vec<ToolDeclaration> {
    let mut admitted = inner_declarations_for(resolved, discovered);
    let discovered_names: HashSet<&str> = admitted
        .iter()
        .map(|declaration| declaration.name.as_str())
        .collect();
    let builtin_admitted: Vec<ToolDeclaration> = resolved
        .iter()
        .filter(|name| !discovered_names.contains(name.as_str()))
        .filter(|name| crate::config::is_bash_inner_tool_allowed(name))
        .filter_map(|name| crate::config::builtin_tool_declaration(name))
        .collect();
    admitted.extend(builtin_admitted);
    admitted.sort_by(|a, b| a.name.cmp(&b.name));
    admitted
}

/// Script and execution options shared by reserved and session operator calls.
pub(crate) struct BashInvocation<'a> {
    pub script: &'a str,
    pub timeout_secs: Option<u64>,
    pub format: ToolOutputFormat,
    pub abort: &'a crate::utils::AbortSignal,
}

/// Evaluate one operator Bash script call (#2630) on a command-scoped
/// context that the caller installed into the bash slot. `owner` stays alive
/// for the whole call, so the provider can fork inner-call contexts from it.
/// Output and error semantics match the direct `Call` path: the reply is
/// `format_tool_result` of the full provider result, and nonzero script
/// status reports failure with the exit code carried in the result.
pub(crate) async fn evaluate_bash(
    ctx: &ToolEvalContext,
    root_call_id: &str,
    invocation: BashInvocation<'_>,
) -> Result<OperatorToolReply> {
    let BashInvocation {
        script,
        timeout_secs,
        format,
        abort,
    } = invocation;
    ensure!(!abort.aborted(), "Operator tool command cancelled");
    let bash_name = crate::config::BASH_TOOL_NAME;
    ensure!(
        ctx.allowed_tool_names.contains(bash_name),
        "Tool '{bash_name}' is not allowed in this context"
    );
    let mut arguments = serde_json::json!({ "script": script });
    if let Some(secs) = timeout_secs.filter(|secs| *secs > 0) {
        arguments["timeout_secs"] = serde_json::json!(secs);
    }
    let call = ToolCall {
        name: bash_name.to_string(),
        arguments,
        id: Some(root_call_id.to_string()),
        thought_signature: None,
        reasoning_provenance: None,
    };
    let evaluated = eval_inner_tool_call(ctx, call, abort).await?;
    let result = evaluated.result;
    let error = result_error(&result.output);
    Ok(OperatorToolReply {
        output: format_tool_result(&result.output, format)?,
        error,
        error_kind: None,
    })
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum OperatorToolMessage {
    Started,
    Finished(OperatorToolReply),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatorToolErrorKind {
    /// The target tool is not available under the session's tool
    /// permissions. Authorization itself is still enforced by the engine;
    /// this kind only classifies the failure for callers (surface actions
    /// map it to `unauthorized_tool` instead of matching error strings).
    UnauthorizedTool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OperatorToolReply {
    pub output: String,
    pub error: Option<String>,
    /// Machine-readable classification of `error`, when the worker knows
    /// one. Absent on every reply an older worker sends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<OperatorToolErrorKind>,
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
/// agent or virtual caller session. All provider and middleware execution stays on workers.
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
        // TUI commands run with the existing operator envelope; the worker
        // admits them with the effective session use_tools plus bash_tools.
        bash_timeout_secs: None,
        // Decision 11: credential-relevant operator calls act for the
        // operator's own identity, not the active session's owner. The same
        // default the CLI stamps on new sessions.
        user_id: Some(snapshot.default_user_id_for_cluster(cluster)),
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
            if declarations.iter().any(|tool| tool.name == *name) {
                format_tool_declaration(find_tool(&declarations, name)?, format)?
            } else {
                // Same typed denial as `call`: callers must not
                // string-match the transport error to detect an
                // unauthorized tool.
                return Ok(unavailable_tool_reply(name));
            }
        }
        OperatorToolCommand::List { pattern } => {
            format_tool_list(&selected_declarations(&declarations, pattern), format)?
        }
        OperatorToolCommand::Call {
            name,
            args_json,
            tool_input,
        } => {
            if !declarations.iter().any(|tool| tool.name == *name) {
                return Ok(unavailable_tool_reply(name));
            }
            let eval = eval_call(ctx, name, args_json, format, abort);
            let reply = match tool_input {
                // Decision 11: a polling `credentials connect` re-call carries
                // its accepted url-challenge response and the tool's request
                // state beside the arguments, scoped to this one dispatch.
                Some(input) => {
                    crate::middleware::with_tool_input(
                        crate::middleware::ToolInputResume {
                            responses: input.responses.clone(),
                            request_state: input.request_state.clone(),
                        },
                        eval,
                    )
                    .await?
                }
                None => eval.await?,
            };
            return Ok(reply);
        }
        // Operator paths evaluate Bash on the command-scoped context
        // (`evaluate_bash`): the script provider needs the slot-installed
        // context, which this by-value entry point cannot provide.
        OperatorToolCommand::Bash { .. } => {
            bail!("Operator bash commands require the command-scoped evaluation path")
        }
    };
    Ok(OperatorToolReply {
        output,
        error: None,
        error_kind: None,
    })
}

/// Typed denial for a tool the session's operator surface does not expose:
/// callers match `error_kind`, never the message string.
fn unavailable_tool_reply(name: &str) -> OperatorToolReply {
    OperatorToolReply {
        output: String::new(),
        error: Some(format!(
            "Tool '{name}' is not available in the active session"
        )),
        error_kind: Some(OperatorToolErrorKind::UnauthorizedTool),
    }
}

/// The declarations a `list` command reports, filtered by its glob pattern.
fn selected_declarations(
    declarations: &[ToolDeclaration],
    pattern: &Option<String>,
) -> Vec<ToolDeclaration> {
    let selector = pattern
        .as_deref()
        .map(|pattern| crate::tool_selector::ToolSelector::new(pattern, true));
    declarations
        .iter()
        .filter(|tool| selector.as_ref().is_none_or(|s| s.is_match(&tool.name)))
        .cloned()
        .collect()
}

/// Dispatch a `call` through the operator tool path. The caller has already
/// confirmed the tool is exposed to this session.
async fn eval_call(
    ctx: ToolEvalContext,
    name: &str,
    args_json: &str,
    format: ToolOutputFormat,
    abort: &crate::utils::AbortSignal,
) -> Result<OperatorToolReply> {
    let call = ToolCall {
        name: name.to_string(),
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
    Ok(OperatorToolReply {
        output: format_tool_result(&result.output, format)?,
        error,
        error_kind: None,
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

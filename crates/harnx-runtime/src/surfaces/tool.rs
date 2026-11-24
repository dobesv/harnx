//! Direct-tool execution for surface actions: the one shared operator-RPC
//! path both the lease-holding worker and the idle client
//! use, so permissions and middleware apply identically and the model-safe
//! summary scrubs user-audience resources.

use crate::surfaces::SurfaceActionOutcome;
use harnx_core::interaction::non_modal;
use serde_json::Value;

/// Operator-RPC routing for a direct surface tool target: the NATS client,
/// the operator subject that owns permissions and middleware, the session the
/// action belongs to, and the worker abort signal.
pub struct SurfaceToolRoute<'a> {
    pub client: &'a async_nats::Client,
    pub operator_subject: &'a str,
    pub session_key: &'a str,
    pub abort_signal: &'a crate::utils::AbortSignal,
}

/// Run a declared direct tool target through the operator RPC and return a
/// model-safe summary of its output.
///
/// Shared by the lease-holding worker and the idle-session client; both
/// must route through the operator RPC so the session's tool permissions and
/// middleware apply, and both must classify a rejected target tool from the
/// typed `error_kind` instead of matching error strings.
/// The summary scrubs user-audience A2UI resource blocks out of the lossless
/// operator output before it can be quoted into model-visible note/turn
/// text; the operator channel itself stays lossless.
pub async fn run_surface_tool(
    route: &SurfaceToolRoute<'_>,
    target: &non_modal::SurfaceToolTarget,
    data: &serde_json::Map<String, Value>,
) -> std::result::Result<String, SurfaceActionOutcome> {
    let arguments = non_modal::tool_arguments(target, data);
    let request = crate::operator_tools::OperatorToolRequest {
        version: 1,
        session_key: route.session_key.to_string(),
        use_tools: None,
        tool_use: true,
        command: crate::operator_tools::OperatorToolCommand::Call {
            name: target.name.clone(),
            args_json: arguments.to_string(),
            tool_input: None,
        },
        json: true,
        bash_timeout_secs: None,
        // Surface actions act inside their session, so they keep the legacy
        // form: no explicit operator identity, and the worker stamps the
        // session owner (decision 11 covers only CLI/TUI operator calls).
        user_id: None,
    };
    let reply = crate::operator_tools::request_command(
        route.client,
        route.operator_subject,
        &request,
        route.abort_signal,
    )
    .await
    .map_err(|error| {
        log::warn!("surface action tool '{}' failed: {error:#}", target.name);
        SurfaceActionOutcome::ToolFailed
    })?;
    if let Some(error) = &reply.error {
        if reply.error_kind == Some(crate::operator_tools::OperatorToolErrorKind::UnauthorizedTool)
        {
            return Err(SurfaceActionOutcome::UnauthorizedTool);
        }
        log::warn!("surface action tool '{}' errored: {error}", target.name);
        return Err(SurfaceActionOutcome::ToolFailed);
    }
    Ok(scrub_tool_summary(&reply.output))
}

/// A model-safe form of a raw operator tool output: parse the JSON envelope
/// and drop user-audience A2UI resource blocks. Non-JSON output passes
/// through unchanged — a text reply carries no resource blocks to scrub.
fn scrub_tool_summary(raw: &str) -> String {
    match serde_json::from_str::<Value>(raw) {
        Ok(mut output) => {
            harnx_engine::a2ui::strip_user_audience_resources(&mut output);
            output.to_string()
        }
        Err(_) => raw.to_string(),
    }
}

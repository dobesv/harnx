//! T4.3 conversion of a raw provider tool challenge into a durable
//! interaction: validation, payload stamping, D6 identity and the tool's
//! checkpoint state. Middleware `confirm` and `interact` produce the same
//! `InteractionRequest` shape, so every frontend and the durable acceptance
//! path work on tool challenges unchanged.
use anyhow::{bail, ensure, Context, Result};
use harnx_core::interaction::{
    confirm_surface, interaction_id, surface, InteractionIdentity, InteractionRequest,
    InteractionStep,
};
use harnx_core::tool::ToolInputRequired;
use harnx_engine::tool::{ToolCallCheckpoint, ToolMiddlewareOutcome};

use super::interact::{stamped_surface, surface_summary};
use super::resume::CheckpointState;

/// Round-scoped facts the provider cannot supply: the durable layer owns
/// them, so a tool cannot forge another round's or call's interactions.
pub(crate) struct ToolChallengeContext {
    pub session: String,
    pub tool_round_seq: u64,
    pub tool_call_id: String,
    pub chain_version: String,
    /// Enclosing middleware `next` steps, empty on a middleware-free call.
    pub step_path: Vec<InteractionStep>,
}

/// Convert a raw provider challenge into the engine's typed suspension
/// outcome, minting its interaction identity and checkpoint together from
/// the same round-scoped facts.
pub(crate) fn tool_challenge_outcome(
    input: &ToolInputRequired,
    context: &ToolChallengeContext,
) -> Result<ToolMiddlewareOutcome> {
    let interaction = tool_challenge_request(input, context)?;
    let checkpoint = tool_challenge_checkpoint(input, context)?;
    Ok(ToolMiddlewareOutcome::InputRequired {
        interaction,
        checkpoint,
    })
}

/// Build the durable interaction request for a raw tool challenge. The
/// producer is infrastructure-derived (`tool:<server>/<tool>`), the key is
/// the tool's own request key, and the rest of the D6 identity applies
/// unchanged. Kind `a2ui` surfaces are validated and stamped here, before
/// the challenge becomes visible to any frontend.
// qual:allow(dry, boilerplate) reason: "The D6 identity is assembled from borrowed wire inputs reused by the checkpoint builder; cloning keeps the conversion read-only instead of threading ownership through Into impls for two call sites."
pub(crate) fn tool_challenge_request(
    input: &ToolInputRequired,
    context: &ToolChallengeContext,
) -> Result<InteractionRequest> {
    validate_challenge(input, context)?;
    let (payload, summary) = match input.kind.as_str() {
        ToolInputRequired::KIND_CONFIRM => {
            let message = input
                .message
                .as_deref()
                .context("missing confirm message")?;
            (
                confirm_surface(&uuid::Uuid::new_v4().to_string(), message),
                message.to_string(),
            )
        }
        ToolInputRequired::KIND_A2UI => {
            let surface = input.surface.clone().context("missing a2ui surface")?;
            surface::validate_surface(&surface)
                .map_err(|error| anyhow::anyhow!("invalid interact surface: {error}"))?;
            let payload = stamped_surface(surface);
            let summary = if input.summary.is_empty() {
                surface_summary(&payload)
            } else {
                input.summary.clone()
            };
            (payload, summary)
        }
        _ => bail!("unsupported tool challenge kind after validation"),
    };
    let request = InteractionRequest {
        interaction_id: String::new(),
        producer: input.producer.clone(),
        step_path: context.step_path.clone(),
        key: input.key.clone(),
        kind: input.kind.clone(),
        payload,
        summary,
        chain_version: context.chain_version.clone(),
    };
    let interaction_id = interaction_id(&InteractionIdentity {
        session: &context.session,
        tool_round_seq: context.tool_round_seq,
        tool_call_id: &context.tool_call_id,
        chain_version: &context.chain_version,
        producer: &request.producer,
        step_path: &request.step_path,
        key: &request.key,
        kind: &request.kind,
        payload: &request.payload,
    });
    Ok(InteractionRequest {
        interaction_id,
        ..request
    })
}

/// The call's checkpoint for a tool challenge: middleware frame state is
/// merged by the chain runner's checkpoint lift (`middleware::wiring`), so
/// this carries only the tool's opaque request state for the rerun.
// qual:allow(dry, boilerplate) reason: "Round-scoped context fields are borrowed into the stored checkpoint; the tool's request state is the only owned payload, cloned once for the rerun."
pub(crate) fn tool_challenge_checkpoint(
    input: &ToolInputRequired,
    context: &ToolChallengeContext,
) -> Result<ToolCallCheckpoint> {
    let state = CheckpointState {
        version: 1,
        session: context.session.clone(),
        tool_round_seq: context.tool_round_seq,
        tool_call_id: context.tool_call_id.clone(),
        chain_version: context.chain_version.clone(),
        frames: vec![],
        tool_request_state: Some(input.request_state.clone()),
    };
    Ok(ToolCallCheckpoint {
        state: serde_json::to_value(state).context("serialize tool challenge checkpoint")?,
        snapshots: Default::default(),
    })
}

/// Runtime-side bounds before suspension; the tool server already enforced
/// the wire shape, and the provider re-validated it, so this is the last
/// gate before the challenge becomes a visible interaction.
fn validate_challenge(input: &ToolInputRequired, context: &ToolChallengeContext) -> Result<()> {
    ensure!(
        input.producer.starts_with("tool:")
            && input
                .producer
                .split_once('/')
                .is_some_and(|(server, tool)| {
                    !server.trim_start_matches("tool:").is_empty() && !tool.is_empty()
                }),
        "tool challenge producer must be tool:<server>/<tool>"
    );
    ensure!(
        context
            .step_path
            .iter()
            .all(|step| !step.middleware.is_empty() && !step.key.is_empty()),
        "tool challenge step path must label middleware next steps"
    );
    ensure!(
        !input.key.is_empty() && input.key.len() <= harnx_toolset::MAX_TOOL_INPUT_KEY_BYTES,
        "tool challenge key exceeds {} bytes",
        harnx_toolset::MAX_TOOL_INPUT_KEY_BYTES
    );
    let state = serde_json::to_vec(&input.request_state).unwrap_or_default();
    ensure!(
        state.len() <= harnx_toolset::MAX_TOOL_REQUEST_STATE_BYTES,
        "tool challenge request_state exceeds {} bytes",
        harnx_toolset::MAX_TOOL_REQUEST_STATE_BYTES
    );
    match input.kind.as_str() {
        ToolInputRequired::KIND_CONFIRM => ensure!(
            input
                .message
                .as_deref()
                .is_some_and(|message| !message.is_empty()),
            "confirm tool challenge needs a message"
        ),
        ToolInputRequired::KIND_A2UI => ensure!(
            input
                .surface
                .as_ref()
                .is_some_and(|surface| !surface.is_null()),
            "a2ui tool challenge needs a surface"
        ),
        other => anyhow::bail!("unsupported tool challenge kind '{other}'"),
    }
    Ok(())
}

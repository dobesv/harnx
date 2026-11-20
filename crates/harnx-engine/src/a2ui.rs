//! Extraction of user-audience A2UI surfaces from tool output (plan T5.1).
//!
//! A completed tool result carries non-modal surfaces as MCP-shaped embedded
//! resources in `output.content`: a block with `type: "resource"` whose
//! resource has MIME type `application/a2ui+json` and whose block annotations
//! address the user (`annotations.audience` contains `"user"`) — the carriage
//! pinned in the A2UI-over-MCP guide (plan note `t4-0-spec-pins`). Extraction
//! moves them out of `output` (the model never sees them) into
//! [`ToolResult::user_surfaces`], mirroring how
//! [`crate::media::prepare_tool_result_images`] moves images into `content`.
//! Extraction must run before image projection: a malformed image replaces
//! `output` with a diagnostic wrapping the raw reply, which would
//! re-expose quarantined user-only surfaces (review B3).

use anyhow::Result;
use harnx_core::interaction::surface::{validate_surface, BASIC_CATALOG_ID};
use serde_json::{json, Value};

/// MIME type of an A2UI surface envelope carried in an embedded resource.
pub const A2UI_MIME_TYPE: &str = "application/a2ui+json";

/// Move user-audience A2UI surface envelopes out of `output.content` into the
/// result's `user_surfaces`.
///
/// A malformed or invalid surface is a projection error. All user-audience
/// resources are quarantined before validation, including invalid siblings.
/// Only a fixed diagnostic reaches the model: validator errors can quote
/// private component ids, property names, or values. Replay uses this same
/// projection; raw replies belong in the provider journal, not model output.
pub(crate) fn prepare_tool_result_surfaces(output: &mut Value) -> Vec<Value> {
    match extract_a2ui_surfaces(output) {
        Ok(surfaces) => surfaces,
        Err(_) => {
            *output = json!({
                "is_error": true,
                "error": "invalid user-audience a2ui surface; resources omitted",
            });
            Vec::new()
        }
    }
}

/// Remove user-audience A2UI resource blocks from a tool output without
/// validating them.
///
/// Used when an operator-path result (which skips extraction and stays
/// lossless) is quoted into model-visible text, such as a surface action's
/// generated note: the raw operator channel keeps the blocks, but a summary
/// derived from the same output must never reintroduce user-only surfaces as
/// model text. Only top-level `content` arrays are scrubbed.
pub fn strip_user_audience_resources(output: &mut Value) {
    let Some(content) = output.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };
    content.retain(|block| !is_user_a2ui_block(block));
}

fn extract_a2ui_surfaces(output: &mut Value) -> Result<Vec<Value>> {
    let Some(blocks) = output.get_mut("content").and_then(Value::as_array_mut) else {
        return Ok(Vec::new());
    };
    // Remove every private block before parsing even the first one.
    let (private, visible): (Vec<_>, Vec<_>) = std::mem::take(blocks)
        .into_iter()
        .partition(is_user_a2ui_block);
    *blocks = visible;
    private.iter().map(parse_surface).collect()
}

/// An embedded-resource block addressed to the user with the A2UI MIME type.
fn is_user_a2ui_block(block: &Value) -> bool {
    if block.get("type").and_then(Value::as_str) != Some("resource") {
        return false;
    }
    let Some(resource) = block.get("resource") else {
        return false;
    };
    let mime = resource
        .get("mimeType")
        .or_else(|| resource.get("mime_type"))
        .and_then(Value::as_str);
    if mime != Some(A2UI_MIME_TYPE) {
        return false;
    }
    block
        .get("annotations")
        .and_then(|annotations| annotations.get("audience"))
        .and_then(Value::as_array)
        .is_some_and(|audience| audience.iter().any(|who| who == "user"))
}

/// Parse and validate the block's surface envelope. The tool authors the
/// `surfaceId` (it is the surface's durable identity); only a missing
/// `catalogId` is defaulted.
fn parse_surface(block: &Value) -> Result<Value> {
    let text = block
        .pointer("/resource/text")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            anyhow::anyhow!("a2ui surface resource has no text field to carry the envelope")
        })?;
    let mut envelope: Value = serde_json::from_str(text)
        .map_err(|error| anyhow::anyhow!("a2ui surface envelope is not valid JSON: {error}"))?;
    validate_surface(&envelope)
        .map_err(|reason| anyhow::anyhow!("invalid a2ui surface: {reason}"))?;
    let create = envelope
        .get_mut("createSurface")
        .ok_or_else(|| anyhow::anyhow!("a2ui surface envelope has no createSurface"))?;
    if create.get("catalogId").is_none() {
        create["catalogId"] = json!(BASIC_CATALOG_ID);
    }
    Ok(envelope)
}

#[cfg(test)]
mod tests;

use super::frame::Frame;
use anyhow::{anyhow, ensure, Result};
use harnx_core::interaction::{response as surface_response, surface, InteractionRequest};
use harnx_engine::tool::{ToolCallCheckpoint, ToolMiddlewareOrigin, ToolMiddlewareOutcome};
use harnx_script::ScriptHostContext;
use serde_json::{json, Value};
use std::sync::Arc;

/// Interaction kind for general A2UI surfaces, next to `confirm`.
/// Also stamped as `harnx.kind` inside `createSurface.metadata.extensions`.
pub(super) const KIND: &str = "a2ui";

/// Frontends show the summary in tight space (tool lines, notifications);
/// cap it so a long headline cannot blow up the display.
const SUMMARY_LIMIT: usize = 200;

impl Frame {
    /// Request an A2UI surface, returning `{action, data, note?}` once answered.
    /// Without an accepted answer bound to exactly this request, the script
    /// suspends. The surface must pass the pinned Basic Catalog subset
    /// validation at request time.
    pub fn interact(
        self: &Arc<Self>,
        message: Value,
        id: Option<String>,
        host: &ScriptHostContext,
    ) -> Result<Value> {
        let _operation = self.begin()?;
        let key = self.interaction_key(id, "interact")?;
        surface::validate_surface(&message)
            .map_err(|error| anyhow!("invalid interact surface: {error}"))?;
        let payload = stamped_surface(message);
        let request = self.interact_request(key, payload);
        if self.origin == ToolMiddlewareOrigin::Inner {
            self.terminal.lock().get_or_insert_with(|| {
                Ok(ToolMiddlewareOutcome::Completed(super::envelope::denial(
                    &self.prepared.call,
                    &self.name,
                    "interaction required, can't be answered inside a script",
                )))
            });
            host.control.stop();
            return Err(anyhow!(
                "interaction required, can't be answered inside a script"
            ));
        }
        // Operators have no interaction UI (#1865); only confirm auto-approves.
        // `interact` never suspends an operator call: there is no pending
        // interaction to answer with `harnx interact answer` afterwards.
        if self.origin == ToolMiddlewareOrigin::Operator {
            return Err(anyhow!(
                "interact cannot be answered on an operator call (no interaction UI); \
                 run the script in a session instead, and answer its pending \
                 interaction with `harnx interact answer`"
            ));
        }
        let accepted = self
            .resume
            .lock()
            .responses
            .iter()
            .rev()
            .find(|accepted| {
                accepted.request.interaction_id == request.interaction_id
                    && self.request_id(&accepted.request) == request.interaction_id
            })
            .cloned();
        if let Some(accepted) = accepted {
            ensure!(
                allows_action(&request, &accepted.response.action),
                "invalid accepted interact action"
            );
            return Ok(serde_json::to_value(&accepted.response)?);
        }
        self.terminal
            .lock()
            .get_or_insert(Ok(ToolMiddlewareOutcome::InputRequired {
                interaction: request,
                checkpoint: ToolCallCheckpoint::default(),
            }));
        host.control.stop();
        Err(anyhow!("middleware input required"))
    }

    fn interact_request(&self, key: String, payload: Value) -> InteractionRequest {
        let summary = surface_summary(&payload);
        let mut request = InteractionRequest {
            interaction_id: String::new(),
            producer: format!("middleware:{}", self.name),
            step_path: self.identity.step_path.clone(),
            key,
            kind: KIND.into(),
            payload,
            summary,
            chain_version: self.resume.lock().state.chain_version.clone(),
        };
        request.interaction_id = self.request_id(&request);
        request
    }
}

/// An accepted action is valid when it names a Button action event in the
/// requested surface, or is `deny`. The same shared check gates durable
/// acceptance; this resume-time check stays as the backstop for responses
/// that reach a frame without passing through acceptance.
fn allows_action(request: &InteractionRequest, action: &str) -> bool {
    action == "deny" || surface_response::action_is_declared(&request.payload, action)
}

/// Stamp runtime-owned transport fields: a fresh surface ID per attempt
/// (surface IDs are unique for a renderer's lifetime and canonicalization
/// drops them from identity), the Basic Catalog id when absent, and the
/// `harnx.kind` extension frontends use to route the interaction.
/// Shared with the T4.3 tool-challenge conversion: tool-supplied A2UI
/// surfaces get the same treatment as middleware `interact` surfaces.
pub(super) fn stamped_surface(mut message: Value) -> Value {
    let create = &mut message["createSurface"];
    create["surfaceId"] = json!(uuid::Uuid::new_v4().to_string());
    if create.get("catalogId").is_none() {
        create["catalogId"] = json!(surface::BASIC_CATALOG_ID);
    }
    create["metadata"]["extensions"]["harnx"] = json!({"kind": KIND});
    message
}

/// Short human-readable summary from the first literal Text component,
/// falling back to a generic label for surfaces without one.
/// Shared with the T4.3 tool-challenge conversion.
pub(super) fn surface_summary(payload: &Value) -> String {
    match first_literal_text(payload) {
        Some(text) => truncate_summary(text),
        None => "A2UI surface".to_string(),
    }
}

/// The first literal Text component's text: the surface's headline.
fn first_literal_text(payload: &Value) -> Option<&str> {
    payload
        .get("createSurface")
        .and_then(|create| create.get("components"))
        .and_then(Value::as_array)
        .and_then(|components| {
            components.iter().find_map(|component| {
                if component["component"] == "Text" {
                    component["text"].as_str()
                } else {
                    None
                }
            })
        })
}

/// Truncates to [`SUMMARY_LIMIT`] bytes on a char boundary, marking the cut.
fn truncate_summary(text: &str) -> String {
    if text.len() <= SUMMARY_LIMIT {
        return text.to_string();
    }
    let cut = char_boundary_before(text, SUMMARY_LIMIT);
    format!("{}…", &text[..cut])
}

/// The largest byte offset at or below `limit` that does not split a
/// character, so `&text[..cut]` is a valid slice.
fn char_boundary_before(text: &str, limit: usize) -> usize {
    let mut cut = limit;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_truncation_cuts_on_char_boundaries() {
        // `€` is three bytes, so byte 200 falls mid-character here.
        // String::truncate would panic there; the cut backs off instead.
        let text = "€".repeat(100);
        let summary = truncate_summary(&text);
        assert!(summary.len() <= SUMMARY_LIMIT + "…".len());
        assert!(summary.ends_with('…'));
        assert_eq!(truncate_summary("short"), "short");
    }
}

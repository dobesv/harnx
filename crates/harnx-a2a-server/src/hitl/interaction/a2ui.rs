//! A2UI v1.0 action envelopes; input values travel in a Harnx extension.
use a2a_lf::{Part, PartContent};
use anyhow::{ensure, Context, Result};
use harnx_core::interaction::{
    response::{build_data, button_actions, input_fields, validate_response},
    text, InteractionResponse,
};
use serde::Deserialize;
use serde_json::{Map, Value};

pub(super) const MIME: &str = "application/a2ui+json";

#[derive(Debug)]
pub(crate) struct InvalidReply(pub String);

// qual:allow(dry, boilerplate) reason: "Typed correction errors need Display; avoid a new derive dependency for one string wrapper."
impl std::fmt::Display for InvalidReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for InvalidReply {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ActionMessage {
    version: String,
    action: Action,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Action {
    name: String,
    surface_id: String,
    source_component_id: String,
    timestamp: String,
    #[serde(rename = "userMessage")]
    _user_message: Option<String>,
    #[serde(rename = "context")]
    _context: Map<String, Value>,
    #[serde(default)]
    metadata: Metadata,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    #[serde(default)]
    extensions: Map<String, Value>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Answer {
    #[serde(default)]
    data: Map<String, Value>,
    note: Option<String>,
}

pub(super) fn parse(payload: &Value, parts: &[Part]) -> Result<InteractionResponse> {
    validated_response(payload, parts).map_err(|error| InvalidReply(error.to_string()).into())
}

fn validated_response(payload: &Value, parts: &[Part]) -> Result<InteractionResponse> {
    let response = parse_parts(payload, parts)?;
    validate_response(payload, &response).map_err(anyhow::Error::msg)?;
    Ok(response)
}

fn parse_parts(payload: &Value, parts: &[Part]) -> Result<InteractionResponse> {
    if let [part] = parts {
        if let PartContent::Data(data) = &part.content {
            ensure!(
                part.media_type.as_deref() == Some(MIME),
                "expected mediaType {MIME}"
            );
            return parse_action(payload, data);
        }
    }
    let mut answer = String::new();
    for part in parts {
        let PartContent::Text(value) = &part.content else {
            anyhow::bail!("send one A2UI DataPart or text only, not mixed parts");
        };
        answer.push_str(value);
    }
    text::parse(payload, &answer).map_err(anyhow::Error::msg)
}

fn parse_action(payload: &Value, data: &Value) -> Result<InteractionResponse> {
    let message: ActionMessage =
        serde_json::from_value(data.clone()).context("expected an A2UI v1.0 action envelope")?;
    ensure!(message.version == "1.0", "expected A2UI version 1.0");
    let action = message.action;
    ensure!(
        payload["createSurface"]["surfaceId"].as_str() == Some(&action.surface_id),
        "action surfaceId does not match the pending surface"
    );
    chrono::DateTime::parse_from_rfc3339(&action.timestamp).context("invalid action timestamp")?;
    check_source(payload, &action)?;
    let answer: Answer = action
        .metadata
        .extensions
        .get("harnx_response")
        .map(|value| serde_json::from_value(value.clone()))
        .transpose()
        .context("invalid harnx_response extension")?
        .unwrap_or_default();
    check_input_keys(payload, &action.name, &answer.data)?;
    // Never trust the renderer's resolved context. The shared builder derives it
    // from the declared event and the supplied input values, as other clients do.
    let data = build_data(payload, &action.name, &answer.data).map_err(anyhow::Error::msg)?;
    Ok(InteractionResponse {
        action: action.name,
        data,
        note: answer.note,
    })
}

fn check_source(payload: &Value, action: &Action) -> Result<()> {
    if action.name == "deny" {
        return Ok(());
    }
    ensure!(
        button_actions(payload)
            .iter()
            .any(|button| button.name == action.name
                && button.component_id == action.source_component_id),
        "action is not declared by sourceComponentId"
    );
    Ok(())
}

fn check_input_keys(payload: &Value, action: &str, values: &Map<String, Value>) -> Result<()> {
    if action == "deny" {
        return Ok(());
    }
    let fields = input_fields(payload);
    for key in values.keys() {
        ensure!(
            fields.iter().any(|field| field.key == *key),
            "unknown input key '{key}'"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests;

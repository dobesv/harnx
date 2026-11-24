//! Non-modal A2UI surfaces on completed tool results (plan T5.1).
//!
//! A completed result may carry user-audience surface envelopes
//! (`ToolResult::user_surfaces`); frontends render them alongside the chat and
//! submit actions on them. Each action declares its visibility and, optionally,
//! a direct tool target through the Button component's
//! `metadata.extensions.harnx`:
//!
//! ```json
//! { "harnx": {
//!     "visibility": "note",
//!     "tool": { "name": "deploy_release", "arguments": { "confirm": true } }
//! } }
//! ```
//!
//! Visibility semantics (R9):
//! - `invisible`: logged only; a durable record with no model effect.
//! - `note`: a passive context line at the next model call, no new turn.
//!   The default for direct tool-call actions.
//! - `turn`: the action's `user_message` starts a turn, or joins the running
//!   one. The default when no tool target is declared.
//!
//! Action `data` uses the same contract as modal `interact` responses
//! ([`crate::interaction::response`]); `deny` is not a surface action.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::response;
use super::InteractionResponse;

pub const VISIBILITY_INVISIBLE: &str = "invisible";
pub const VISIBILITY_NOTE: &str = "note";
pub const VISIBILITY_TURN: &str = "turn";

/// Cap on the generated (non-`user_message`) part of a note or turn text, so a
/// huge action `data` payload cannot flood the next model call.
pub const ACTION_TEXT_CAP: usize = 500;

/// The effective visibility of one surface action.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceVisibility {
    Invisible,
    Note,
    Turn,
}

impl SurfaceVisibility {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Invisible => VISIBILITY_INVISIBLE,
            Self::Note => VISIBILITY_NOTE,
            Self::Turn => VISIBILITY_TURN,
        }
    }
}

/// A direct tool target declared by an action: the runtime routes the call
/// through the operator-style path, so the agent's tool permissions and
/// middleware apply (D2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SurfaceToolTarget {
    pub name: String,
    /// Base arguments; the submitted action `data` overrides these keys.
    pub arguments: serde_json::Map<String, Value>,
}

/// The declaration read off one Button of a published surface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SurfaceActionDeclaration {
    pub visibility: SurfaceVisibility,
    pub tool: Option<SurfaceToolTarget>,
}

/// Why a submitted surface action was rejected. Nothing is recorded or
/// dispatched for a rejection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SurfaceActionError {
    /// Not a Button `action.event.name` declared on this surface (`deny`
    /// included: a surface has no dismissal action).
    UndeclaredAction,
    /// The declaration names a visibility outside the three allowed values.
    InvalidVisibility(String),
    /// The declaration names a malformed direct tool target.
    InvalidTool(String),
    /// The submitted `data` does not match the surface's bound inputs and the
    /// chosen action's context keys.
    InvalidData(String),
}

/// Validate a submitted action against the published surface and return its
/// declaration (visibility and optional tool target).
pub fn validate_surface_action(
    payload: &Value,
    response: &InteractionResponse,
) -> Result<SurfaceActionDeclaration, SurfaceActionError> {
    if response.action == "deny" || !response::action_is_declared(payload, &response.action) {
        return Err(SurfaceActionError::UndeclaredAction);
    }
    let declaration = action_declaration(payload, &response.action)?
        .ok_or(SurfaceActionError::UndeclaredAction)?;
    response::validate_response(payload, response).map_err(SurfaceActionError::InvalidData)?;
    Ok(declaration)
}

/// The Button component declaring `action` and its `metadata.extensions.harnx`
/// declaration. `None` when the surface declares no such action.
///
/// The default visibility is `note` for direct tool-call actions (the pinned
/// R9 default) and `turn` otherwise.
pub fn action_declaration(
    payload: &Value,
    action: &str,
) -> Result<Option<SurfaceActionDeclaration>, SurfaceActionError> {
    let Some(component) = response::components(payload).iter().find(|component| {
        component["component"] == "Button"
            && component["action"]["event"]["name"].as_str() == Some(action)
    }) else {
        return Ok(None);
    };
    let Some(harnx) = harnx_extension(component) else {
        // No declaration: apply the defaults.
        return Ok(Some(SurfaceActionDeclaration {
            visibility: SurfaceVisibility::Turn,
            tool: None,
        }));
    };
    let tool = match harnx.get("tool") {
        None | Some(Value::Null) => None,
        Some(tool) => Some(parse_tool_target(tool)?),
    };
    let visibility = match harnx.get("visibility") {
        None | Some(Value::Null) => {
            if tool.is_some() {
                SurfaceVisibility::Note
            } else {
                SurfaceVisibility::Turn
            }
        }
        Some(value) => parse_visibility(value)?,
    };
    Ok(Some(SurfaceActionDeclaration { visibility, tool }))
}

/// The `metadata.extensions.harnx` object of a component, when present.
fn harnx_extension(component: &Value) -> Option<&Value> {
    component
        .get("metadata")
        .and_then(|metadata| metadata.get("extensions"))
        .and_then(|extensions| extensions.get("harnx"))
        .filter(|harnx| harnx.is_object())
}

fn parse_visibility(value: &Value) -> Result<SurfaceVisibility, SurfaceActionError> {
    let name = value.as_str().ok_or_else(|| {
        SurfaceActionError::InvalidVisibility(format!(
            "invalid harnx.visibility: expected a string, got {value}"
        ))
    })?;
    match name {
        VISIBILITY_INVISIBLE => Ok(SurfaceVisibility::Invisible),
        VISIBILITY_NOTE => Ok(SurfaceVisibility::Note),
        VISIBILITY_TURN => Ok(SurfaceVisibility::Turn),
        other => Err(SurfaceActionError::InvalidVisibility(format!(
            "invalid harnx.visibility '{other}': expected one of invisible, note, turn"
        ))),
    }
}

fn parse_tool_target(value: &Value) -> Result<SurfaceToolTarget, SurfaceActionError> {
    let object = value.as_object().ok_or_else(|| {
        SurfaceActionError::InvalidTool(format!(
            "invalid harnx.tool: expected an object, got {value}"
        ))
    })?;
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| {
            SurfaceActionError::InvalidTool(
                "invalid harnx.tool: 'name' must be a non-empty string".into(),
            )
        })?
        .to_string();
    let arguments = match object.get("arguments") {
        None | Some(Value::Null) => serde_json::Map::new(),
        Some(arguments) => arguments.as_object().cloned().ok_or_else(|| {
            SurfaceActionError::InvalidTool(
                "invalid harnx.tool: 'arguments' must be an object".into(),
            )
        })?,
    };
    Ok(SurfaceToolTarget { name, arguments })
}

/// Merge the submitted action `data` over the declared base arguments. Data
/// keys override: the tool chooses its binding keys, so collisions are the
/// tool's own declaration.
pub fn tool_arguments(target: &SurfaceToolTarget, data: &serde_json::Map<String, Value>) -> Value {
    let mut arguments = target.arguments.clone();
    for (key, value) in data {
        arguments.insert(key.clone(), value.clone());
    }
    Value::Object(arguments)
}

/// The model-facing or turn-facing text for an accepted action.
///
/// `user_message` wins when present (it is the A2UI action's `userMessage`);
/// otherwise a generated line names the action, its data, and the direct
/// tool's result summary when the action routed to a tool.
pub fn action_text(
    action: &str,
    data: &serde_json::Map<String, Value>,
    user_message: Option<&str>,
    tool_summary: Option<&str>,
) -> String {
    let user_message = user_message
        .map(str::trim)
        .filter(|message| !message.is_empty());
    if let Some(message) = user_message {
        return message.to_string();
    }
    let mut text = format!("[surface action '{action}']");
    if let Some(summary) = tool_summary.map(str::trim).filter(|s| !s.is_empty()) {
        text.push_str(": ");
        text.push_str(&truncate(summary));
    } else if !data.is_empty() {
        let compact = serde_json::to_string(data).unwrap_or_default();
        text.push_str(": ");
        text.push_str(&truncate(&compact));
    }
    text
}

fn truncate(text: &str) -> String {
    text.chars().take(ACTION_TEXT_CAP).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn surface() -> Value {
        json!({
            "version": "1.0",
            "createSurface": {
                "surfaceId": "s-1",
                "components": [
                    { "id": "root", "component": "Card", "child": "col" },
                    { "id": "col", "component": "Column", "children": [
                        "title", "go_label", "go", "plain_label", "plain",
                        "chat_label", "chat", "hidden_label", "hidden"
                    ] },
                    { "id": "title", "component": "Text", "text": "Deploy?" },
                    { "id": "go_label", "component": "Text", "text": "Go" },
                    {
                        "id": "go", "component": "Button", "child": "go_label",
                        "action": { "event": { "name": "deploy" } },
                        "metadata": { "extensions": { "harnx": {
                            "visibility": "note",
                            "tool": { "name": "deploy_release", "arguments": { "confirm": true } }
                        } } }
                    },
                    { "id": "plain_label", "component": "Text", "text": "Tell" },
                    {
                        "id": "plain", "component": "Button", "child": "plain_label",
                        "action": { "event": { "name": "tell" } }
                    },
                    { "id": "chat_label", "component": "Text", "text": "Discuss" },
                    {
                        "id": "chat", "component": "Button", "child": "chat_label",
                        "action": { "event": { "name": "discuss" } },
                        "metadata": { "extensions": { "harnx": { "visibility": "turn" } } }
                    },
                    { "id": "hidden_label", "component": "Text", "text": "Hide" },
                    {
                        "id": "hidden", "component": "Button", "child": "hidden_label",
                        "action": { "event": { "name": "hide" } },
                        "metadata": { "extensions": { "harnx": { "visibility": "invisible" } } }
                    }
                ]
            }
        })
    }

    fn response(action: &str, data: serde_json::Map<String, Value>) -> InteractionResponse {
        InteractionResponse {
            action: action.into(),
            data,
            note: None,
        }
    }

    #[test]
    fn declared_visibility_and_tool_target_are_parsed() {
        let payload = surface();
        let declaration = action_declaration(&payload, "deploy").unwrap().unwrap();
        assert_eq!(declaration.visibility, SurfaceVisibility::Note);
        let tool = declaration.tool.unwrap();
        assert_eq!(tool.name, "deploy_release");
        assert_eq!(tool.arguments.get("confirm"), Some(&json!(true)));
    }

    #[test]
    fn defaults_are_note_for_tool_actions_and_turn_otherwise() {
        let payload = surface();
        let undeclared_tool = action_declaration(&payload, "tell").unwrap().unwrap();
        assert_eq!(undeclared_tool.visibility, SurfaceVisibility::Turn);
        assert!(undeclared_tool.tool.is_none());
        let chat = action_declaration(&payload, "discuss").unwrap().unwrap();
        assert_eq!(chat.visibility, SurfaceVisibility::Turn);
        let hidden = action_declaration(&payload, "hide").unwrap().unwrap();
        assert_eq!(hidden.visibility, SurfaceVisibility::Invisible);
    }

    #[test]
    fn unknown_action_has_no_declaration() {
        let payload = surface();
        assert!(action_declaration(&payload, "nope").unwrap().is_none());
    }

    #[test]
    fn invalid_visibility_is_rejected() {
        let payload = json!({
            "createSurface": { "components": [
                { "id": "bad_label", "component": "Text", "text": "Go" },
                { "id": "bad", "component": "Button", "child": "bad_label",
                  "action": { "event": { "name": "go" } },
                  "metadata": { "extensions": { "harnx": { "visibility": "loud" } } } }
            ] }
        });
        let error = match action_declaration(&payload, "go") {
            Err(SurfaceActionError::InvalidVisibility(message)) => message,
            other => panic!("expected an invalid visibility error, got {other:?}"),
        };
        assert!(error.contains("invalid harnx.visibility 'loud'"));
    }

    #[test]
    fn validation_rejects_undeclared_and_deny_actions() {
        let payload = surface();
        assert_eq!(
            validate_surface_action(&payload, &response("nope", Default::default())),
            Err(SurfaceActionError::UndeclaredAction)
        );
        assert_eq!(
            validate_surface_action(&payload, &response("deny", Default::default())),
            Err(SurfaceActionError::UndeclaredAction)
        );
    }

    #[test]
    fn validation_rejects_mismatched_data() {
        let payload = json!({
            "createSurface": { "components": [
                { "id": "field", "component": "TextField",
                  "value": { "@path": "/name" },
                  "label": "Name" },
                { "id": "go_label", "component": "Text", "text": "Go" },
                { "id": "go", "component": "Button", "child": "go_label",
                  "action": { "event": { "name": "go" } } }
            ] }
        });
        let mut data = serde_json::Map::new();
        data.insert("name".into(), json!(7));
        assert!(matches!(
            validate_surface_action(&payload, &response("go", data)),
            Err(SurfaceActionError::InvalidData(_))
        ));
        let mut unknown = serde_json::Map::new();
        unknown.insert("surprise".into(), json!(1));
        assert!(matches!(
            validate_surface_action(&payload, &response("go", unknown)),
            Err(SurfaceActionError::InvalidData(_))
        ));
    }

    #[test]
    fn tool_arguments_merge_data_over_base() {
        let target = SurfaceToolTarget {
            name: "deploy_release".into(),
            arguments: json!({ "confirm": true, "keep": "base" })
                .as_object()
                .unwrap()
                .clone(),
        };
        let mut data = serde_json::Map::new();
        data.insert("keep".into(), json!("override"));
        data.insert("release".into(), json!("v2"));
        assert_eq!(
            tool_arguments(&target, &data),
            json!({ "confirm": true, "keep": "override", "release": "v2" })
        );
    }

    #[test]
    fn action_text_prefers_user_message_and_caps_generated_text() {
        let mut data = serde_json::Map::new();
        data.insert("payload".into(), json!("x".repeat(2000)));
        let text = action_text("deploy", &data, None, None);
        assert!(text.starts_with("[surface action 'deploy']:"));
        assert!(text.len() < ACTION_TEXT_CAP + 64);
        assert_eq!(
            action_text("deploy", &data, Some("do it"), None),
            "do it".to_string()
        );
        let with_tool = action_text("deploy", &data, None, Some("released v2"));
        assert_eq!(with_tool, "[surface action 'deploy']: released v2");
    }
}

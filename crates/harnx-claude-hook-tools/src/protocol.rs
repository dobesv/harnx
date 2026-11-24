//! Claude's wire format stays here, separate from harnx's legacy hook registry.
use anyhow::{bail, ensure, Result};
use serde::Serialize;
use serde_json::{json, Value};

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Deny,
    Ask,
}

/// Transport-independent result consumed by middleware and credential adapters.
#[derive(Debug, Serialize)]
pub struct HookRunResult {
    pub decision: Decision,
    pub reason: Option<String>,
    pub tool_input: Option<Value>,
    pub tool_response: Option<Value>,
    pub context: Vec<String>,
    /// Original Claude response, including fields used by lifecycle hooks.
    pub result: Value,
}

impl HookRunResult {
    pub(crate) fn blocked(reason: String) -> Self {
        Self {
            decision: Decision::Deny,
            reason: Some(reason),
            tool_input: None,
            tool_response: None,
            context: vec![],
            result: json!({}),
        }
    }
}

pub(crate) fn parse_stdout(bytes: &[u8]) -> Result<HookRunResult> {
    let text = std::str::from_utf8(bytes)?.trim();
    if text.is_empty() {
        return parse_result(json!({}));
    }
    match serde_json::from_str(text) {
        Ok(value) => parse_result(value),
        Err(error) if text.starts_with(['{', '[']) => Err(error.into()),
        // Claude one-shot hooks may emit ordinary text as additional context.
        Err(_) => parse_result(json!({"additionalContext": text})),
    }
}

pub(crate) fn parse_result(result: Value) -> Result<HookRunResult> {
    ensure!(result.is_object(), "hook response must be a JSON object");
    let specific = &result["hookSpecificOutput"];
    ensure!(
        specific.is_null() || specific.is_object(),
        "hookSpecificOutput must be an object"
    );
    let (decision, reason) = decision(&result)?;
    let tool_input = first_value(&[
        &result["mutatedToolInput"],
        &specific["updatedInput"],
        &specific["toolInput"],
    ]);
    ensure!(
        tool_input.as_ref().is_none_or(Value::is_object),
        "mutated tool input must be an object"
    );
    let tool_response = first_value(&[
        &result["mutatedToolResponse"],
        &specific["updatedMCPToolOutput"],
        &specific["toolResponse"],
    ]);
    let context = [
        text(&result, "additionalContext")?,
        text(specific, "additionalContext")?,
        text(&result, "systemMessage")?,
    ]
    .into_iter()
    .flatten()
    .filter(|text| !text.is_empty())
    .collect();
    Ok(HookRunResult {
        decision,
        reason,
        tool_input,
        tool_response,
        context,
        result,
    })
}

fn first_value(values: &[&Value]) -> Option<Value> {
    values
        .iter()
        .find(|value| !value.is_null())
        .map(|v| (*v).clone())
}

fn text(value: &Value, key: &str) -> Result<Option<String>> {
    match &value[key] {
        Value::Null => Ok(None),
        Value::String(text) => Ok(Some(text.clone())),
        _ => bail!("hook response {key} must be a string"),
    }
}

fn decision(result: &Value) -> Result<(Decision, Option<String>)> {
    let permission = text(&result["hookSpecificOutput"], "permissionDecision")?;
    let reason = text(&result["hookSpecificOutput"], "permissionDecisionReason")?
        .or(text(result, "reason")?);
    let legacy = text(result, "decision")?;
    ensure!(
        result["continue"].is_null() || result["continue"].is_boolean(),
        "hook response continue must be a boolean"
    );
    if result["continue"] == false || matches!(legacy.as_deref(), Some("block" | "deny")) {
        return Ok((Decision::Deny, text(result, "stopReason")?.or(reason)));
    }
    let legacy_permission = match legacy.as_deref() {
        Some("approve" | "allow") => Some("allow"),
        Some("ask") => Some("ask"),
        None => None,
        Some(other) => bail!("unknown hook decision: {other}"),
    };
    let decision = match permission.as_deref().or(legacy_permission) {
        Some("deny") => Decision::Deny,
        Some("ask") => Decision::Ask,
        Some("allow") | None => Decision::Allow,
        Some(other) => bail!("unknown hook permissionDecision: {other}"),
    };
    Ok((decision, reason))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_current_and_legacy_fields() {
        for input in ["updatedInput", "toolInput"] {
            for output in ["updatedMCPToolOutput", "toolResponse"] {
                let result = parse_result(json!({"hookSpecificOutput": {
                    "permissionDecision": "ask", "permissionDecisionReason": "Review?",
                    input: {"command":"safe"}, output: {"ok":true}, "additionalContext":"nested"
                }, "additionalContext":"top", "systemMessage":"system"}))
                .unwrap();
                assert!(matches!(result.decision, Decision::Ask));
                assert_eq!(result.reason.as_deref(), Some("Review?"));
                assert_eq!(result.tool_input, Some(json!({"command":"safe"})));
                assert_eq!(result.tool_response, Some(json!({"ok":true})));
                assert_eq!(result.context, ["top", "nested", "system"]);
            }
        }
    }

    #[test]
    fn deny_and_top_level_mutations_take_precedence() {
        let result = parse_result(json!({"continue":false, "stopReason":"stop",
            "mutatedToolInput":{"top":true}, "mutatedToolResponse":false,
            "hookSpecificOutput":{"permissionDecision":"allow", "toolInput":{"old":true}}}))
        .unwrap();
        assert!(matches!(result.decision, Decision::Deny));
        assert_eq!(result.reason.as_deref(), Some("stop"));
        assert_eq!(result.tool_input, Some(json!({"top":true})));
        assert_eq!(result.tool_response, Some(json!(false)));
        assert!(matches!(
            parse_result(json!({"decision":"block"})).unwrap().decision,
            Decision::Deny
        ));
    }

    #[test]
    fn legacy_decisions_fail_closed_and_continue_is_typed() {
        for decision in ["block", "deny"] {
            assert!(matches!(
                parse_result(json!({"decision":decision})).unwrap().decision,
                Decision::Deny
            ));
        }
        for decision in ["approve", "allow"] {
            assert!(matches!(
                parse_result(json!({"decision":decision})).unwrap().decision,
                Decision::Allow
            ));
        }
        assert!(matches!(
            parse_result(json!({"decision":"ask"})).unwrap().decision,
            Decision::Ask
        ));
        for value in [json!({"decision":"reject"}), json!({"continue":"false"})] {
            assert!(parse_result(value).is_err());
        }
    }

    #[test]
    fn empty_and_text_stdout_are_context_but_broken_json_is_error() {
        assert!(parse_stdout(b" \r\n").unwrap().context.is_empty());
        assert_eq!(parse_stdout(b"hello\n").unwrap().context, ["hello"]);
        for bytes in [b"{broken".as_slice(), b"[]", b"\xff"] {
            assert!(parse_stdout(bytes).is_err());
        }
        for value in [
            json!({"hookSpecificOutput":false}),
            json!({"additionalContext":1}),
            json!({"hookSpecificOutput":{"permissionDecision":"bogus"}}),
            json!({"hookSpecificOutput":{"updatedInput":false}}),
        ] {
            assert!(parse_result(value).is_err());
        }
    }
}

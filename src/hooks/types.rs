use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize)]
pub struct HookPayload {
    pub session_id: String,
    pub cwd: PathBuf,
    #[serde(default)]
    pub resume_count: u32,
    #[serde(flatten)]
    pub hook_event: HookEvent,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "hook_event_name", rename_all = "PascalCase")]
pub enum HookEvent {
    SessionStart {
        source: String,
        model: String,
    },
    SessionEnd {
        reason: String,
    },
    UserPromptSubmit {
        prompt: String,
    },
    Stop {
        stop_hook_active: bool,
        last_assistant_message: Option<String>,
    },
    StopFailure {
        error: String,
        error_type: String,
    },
    PreToolUse {
        tool_name: String,
        tool_input: Value,
        tool_use_id: String,
    },
    PostToolUse {
        tool_name: String,
        tool_input: Value,
        tool_response: Value,
        tool_use_id: String,
    },
    PostToolUseFailure {
        tool_name: String,
        tool_input: Value,
        tool_use_id: String,
        error: String,
    },
}

impl HookEvent {
    pub fn event_name(&self) -> &'static str {
        match self {
            Self::SessionStart { .. } => "SessionStart",
            Self::SessionEnd { .. } => "SessionEnd",
            Self::UserPromptSubmit { .. } => "UserPromptSubmit",
            Self::Stop { .. } => "Stop",
            Self::StopFailure { .. } => "StopFailure",
            Self::PreToolUse { .. } => "PreToolUse",
            Self::PostToolUse { .. } => "PostToolUse",
            Self::PostToolUseFailure { .. } => "PostToolUseFailure",
        }
    }

    pub fn matcher_text(&self) -> Option<&str> {
        match self {
            Self::PreToolUse { tool_name, .. }
            | Self::PostToolUse { tool_name, .. }
            | Self::PostToolUseFailure { tool_name, .. } => Some(tool_name),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookResultControl {
    Continue,
    Block { reason: String },
    Ask { reason: Option<String> },
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HookSpecificOutput {
    #[serde(default)]
    pub permission_decision: Option<String>,
    #[serde(default)]
    pub permission_decision_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HookResult {
    #[serde(default)]
    pub additional_context: Option<String>,
    #[serde(default)]
    pub resume: Option<bool>,
    #[serde(default)]
    pub system_message: Option<String>,
    #[serde(default, rename = "hookSpecificOutput")]
    pub hook_specific_output: Option<HookSpecificOutput>,
}

#[derive(Debug, Clone)]
pub struct HookOutcome {
    pub control: HookResultControl,
    pub result: HookResult,
}

#[cfg(test)]
mod tests {
    use super::{HookEvent, HookPayload, HookResult, HookSpecificOutput};
    use serde_json::{json, Value};
    use std::path::PathBuf;

    #[test]
    fn test_hook_payload_serialization() {
        let payload = HookPayload {
            session_id: "session-123".to_string(),
            cwd: PathBuf::from("/tmp/project"),
            resume_count: 2,
            hook_event: HookEvent::PreToolUse {
                tool_name: "shell".to_string(),
                tool_input: json!({"command": "echo hi"}),
                tool_use_id: "call-1".to_string(),
            },
        };

        let value = serde_json::to_value(payload).expect("serialize hook payload");
        assert_eq!(
            value["hook_event_name"],
            Value::String("PreToolUse".to_string())
        );
        assert_eq!(value["tool_name"], Value::String("shell".to_string()));
        assert_eq!(value["tool_input"], json!({"command": "echo hi"}));
        assert_eq!(
            value["session_id"],
            Value::String("session-123".to_string())
        );
        assert_eq!(value["cwd"], Value::String("/tmp/project".to_string()));
        assert_eq!(value["resume_count"], Value::from(2));
    }

    #[test]
    fn test_hook_result_deserialization() {
        let result: HookResult =
            serde_json::from_str(r#"{"resume":true,"additionalContext":"keep going"}"#)
                .expect("deserialize hook result");

        assert_eq!(result.resume, Some(true));
        assert_eq!(result.additional_context.as_deref(), Some("keep going"));
    }

    #[test]
    fn test_hook_result_empty_json() {
        let result: HookResult = serde_json::from_str("{}").expect("deserialize empty hook result");

        assert!(result.additional_context.is_none());
        assert!(result.resume.is_none());
    }

    #[test]
    fn test_event_name() {
        let events = vec![
            (
                HookEvent::SessionStart {
                    source: "cli".to_string(),
                    model: "claude".to_string(),
                },
                "SessionStart",
            ),
            (
                HookEvent::SessionEnd {
                    reason: "complete".to_string(),
                },
                "SessionEnd",
            ),
            (
                HookEvent::UserPromptSubmit {
                    prompt: "hello".to_string(),
                },
                "UserPromptSubmit",
            ),
            (
                HookEvent::Stop {
                    stop_hook_active: true,
                    last_assistant_message: Some("done".to_string()),
                },
                "Stop",
            ),
            (
                HookEvent::StopFailure {
                    error: "boom".to_string(),
                    error_type: "runtime".to_string(),
                },
                "StopFailure",
            ),
            (
                HookEvent::PreToolUse {
                    tool_name: "shell".to_string(),
                    tool_input: json!({}),
                    tool_use_id: "call-1".to_string(),
                },
                "PreToolUse",
            ),
            (
                HookEvent::PostToolUse {
                    tool_name: "shell".to_string(),
                    tool_input: json!({}),
                    tool_response: json!({"ok": true}),
                    tool_use_id: "call-2".to_string(),
                },
                "PostToolUse",
            ),
            (
                HookEvent::PostToolUseFailure {
                    tool_name: "shell".to_string(),
                    tool_input: json!({}),
                    tool_use_id: "call-3".to_string(),
                    error: "failed".to_string(),
                },
                "PostToolUseFailure",
            ),
        ];

        for (event, expected_name) in events {
            assert_eq!(event.event_name(), expected_name);
        }
    }

    #[test]
    fn test_hook_specific_output_deserialization() {
        let output: HookSpecificOutput = serde_json::from_str(
            r#"{"permissionDecision":"deny","permissionDecisionReason":"blocked","hookEventName":"PreToolUse"}"#,
        )
        .expect("deserialize hook specific output");

        assert_eq!(output.permission_decision.as_deref(), Some("deny"));
        assert_eq!(
            output.permission_decision_reason.as_deref(),
            Some("blocked")
        );
    }

    #[test]
    fn test_hook_specific_output_partial() {
        let output: HookSpecificOutput = serde_json::from_str(r#"{"permissionDecision":"ask"}"#)
            .expect("deserialize partial hook specific output");

        assert_eq!(output.permission_decision.as_deref(), Some("ask"));
        assert!(output.permission_decision_reason.is_none());
    }

    #[test]
    fn test_hook_result_with_hook_specific_output() {
        let result: HookResult = serde_json::from_str(
            r#"{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"dangerous"},"additionalContext":"extra"}"#,
        )
        .expect("deserialize hook result with hook specific output");

        assert_eq!(result.additional_context.as_deref(), Some("extra"));
        assert!(result.hook_specific_output.is_some());
        let hso = result.hook_specific_output.unwrap();
        assert_eq!(hso.permission_decision.as_deref(), Some("deny"));
        assert_eq!(hso.permission_decision_reason.as_deref(), Some("dangerous"));
    }

    #[test]
    fn test_hook_result_backward_compat_no_hook_specific_output() {
        let result: HookResult =
            serde_json::from_str(r#"{"resume":true,"additionalContext":"keep going"}"#)
                .expect("deserialize hook result without hook specific output");

        assert_eq!(result.resume, Some(true));
        assert_eq!(result.additional_context.as_deref(), Some("keep going"));
        assert!(result.hook_specific_output.is_none());
    }
}

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

// --- HookConfig / HooksConfig (serialized shape read from config.yaml) -------

/// Default timeout for hook execution in seconds
fn default_timeout() -> Option<u64> {
    Some(30)
}

/// Configuration for a single hook entry
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HookConfig {
    /// Hook event name (e.g., "PreToolUse", "Stop")
    pub event: String,

    /// Optional regex pattern to match against tool_name (for tool-bearing events)
    #[serde(default)]
    pub matcher: Option<String>,

    /// Shell command to execute
    pub command: String,

    /// Timeout in seconds (default 30)
    #[serde(default = "default_timeout")]
    pub timeout: Option<u64>,

    /// Optional status message to display
    #[serde(default)]
    pub status_message: Option<String>,

    #[serde(default, rename = "async")]
    pub async_hook: Option<bool>,

    /// Hook type. Determines the execution protocol.
    /// Supported: "claude-command" (subprocess with stdin/stdout JSON).
    /// Unknown types are silently skipped.
    #[serde(rename = "type")]
    pub hook_type: String,
}

impl HookConfig {
    /// Check if the hook type is supported
    pub fn is_supported_type(&self) -> bool {
        matches!(
            self.hook_type.as_str(),
            "claude-command" | "claude-command-persistent"
        )
    }
}

/// Configuration for all hooks (global or per-agent)
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct HooksConfig {
    /// Maximum number of resume iterations
    #[serde(default)]
    pub max_resume: Option<u32>,

    /// List of hook entries
    #[serde(default)]
    pub entries: Vec<HookConfig>,
}

impl HooksConfig {
    /// Merge global and agent hooks
    ///
    /// Rules:
    /// - Agent entries extend global entries (both lists are combined)
    /// - If agent and global have entries with the same `event` AND same `matcher`, agent takes priority (replaces)
    /// - `max_resume`: agent value overrides global if agent value is Some
    pub fn merge(global: &HooksConfig, agent: &HooksConfig) -> HooksConfig {
        // Start with global entries
        let mut merged_entries = global.entries.clone();

        // Process agent entries
        for agent_entry in &agent.entries {
            // Check if there's a matching entry in global (same event and matcher)
            if let Some(pos) = merged_entries
                .iter()
                .position(|e| e.event == agent_entry.event && e.matcher == agent_entry.matcher)
            {
                // Replace the global entry with the agent entry
                merged_entries[pos] = agent_entry.clone();
            } else {
                // No conflict, add the agent entry
                merged_entries.push(agent_entry.clone());
            }
        }

        // Determine max_resume: agent overrides global if Some
        let max_resume = agent.max_resume.or(global.max_resume);

        HooksConfig {
            max_resume,
            entries: merged_entries,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{HookConfig, HookEvent, HookPayload, HookResult, HookSpecificOutput, HooksConfig};
    use serde_json::{json, Value};
    use std::path::PathBuf;

    #[test]
    fn test_hooks_config_parse() {
        let yaml = r#"
max_resume: 3
entries:
  - event: Stop
    command: "/path/to/hook.sh"
    async: true
    timeout: 10
    type: claude-command
"#;

        let config: HooksConfig = serde_yaml::from_str(yaml).expect("parse hooks config");

        assert_eq!(config.max_resume, Some(3));
        assert_eq!(config.entries.len(), 1);

        let entry = &config.entries[0];
        assert_eq!(entry.event, "Stop");
        assert_eq!(entry.command, "/path/to/hook.sh");
        assert_eq!(entry.timeout, Some(10));
        assert_eq!(entry.hook_type, "claude-command");
        assert!(entry.matcher.is_none());
        assert!(entry.status_message.is_none());
        assert_eq!(entry.async_hook, Some(true));
    }

    #[test]
    fn test_hooks_config_merge() {
        let global = HooksConfig {
            max_resume: Some(5),
            entries: vec![
                HookConfig {
                    event: "Stop".to_string(),
                    matcher: None,
                    command: "global-stop.sh".to_string(),
                    timeout: Some(30),
                    status_message: None,
                    async_hook: None,
                    hook_type: "claude-command".to_string(),
                },
                HookConfig {
                    event: "SessionStart".to_string(),
                    matcher: None,
                    command: "global-start.sh".to_string(),
                    timeout: Some(30),
                    status_message: None,
                    async_hook: None,
                    hook_type: "claude-command".to_string(),
                },
            ],
        };

        let agent = HooksConfig {
            max_resume: Some(3),
            entries: vec![HookConfig {
                event: "PreToolUse".to_string(),
                matcher: Some("shell".to_string()),
                command: "agent-tool.sh".to_string(),
                timeout: Some(15),
                status_message: None,
                async_hook: None,
                hook_type: "claude-command".to_string(),
            }],
        };

        let merged = HooksConfig::merge(&global, &agent);

        // Agent max_resume should win
        assert_eq!(merged.max_resume, Some(3));

        // Should have 3 entries: 2 from global + 1 from agent
        assert_eq!(merged.entries.len(), 3);

        // Check that all events are present
        let events: Vec<&str> = merged.entries.iter().map(|e| e.event.as_str()).collect();
        assert!(events.contains(&"Stop"));
        assert!(events.contains(&"SessionStart"));
        assert!(events.contains(&"PreToolUse"));
    }

    #[test]
    fn test_hooks_config_merge_conflict() {
        let global = HooksConfig {
            max_resume: Some(5),
            entries: vec![HookConfig {
                event: "PreToolUse".to_string(),
                matcher: Some("shell".to_string()),
                command: "global-shell.sh".to_string(),
                timeout: Some(30),
                status_message: None,
                async_hook: None,
                hook_type: "claude-command".to_string(),
            }],
        };

        let agent = HooksConfig {
            max_resume: None,
            entries: vec![HookConfig {
                event: "PreToolUse".to_string(),
                matcher: Some("shell".to_string()),
                command: "agent-shell.sh".to_string(),
                timeout: Some(10),
                status_message: Some("Agent override".to_string()),
                async_hook: None,
                hook_type: "claude-command".to_string(),
            }],
        };

        let merged = HooksConfig::merge(&global, &agent);

        // Global max_resume should be used (agent is None)
        assert_eq!(merged.max_resume, Some(5));

        // Should have only 1 entry (agent replaced global)
        assert_eq!(merged.entries.len(), 1);

        let entry = &merged.entries[0];
        assert_eq!(entry.command, "agent-shell.sh");
        assert_eq!(entry.timeout, Some(10));
        assert_eq!(entry.status_message, Some("Agent override".to_string()));
    }

    #[test]
    fn test_hooks_config_default() {
        let config = HooksConfig::default();

        assert!(config.max_resume.is_none());
        assert!(config.entries.is_empty());
    }

    #[test]
    fn test_supported_type() {
        // None should be valid (defaults to "claude-command")
        let hook1 = HookConfig {
            event: "Stop".to_string(),
            matcher: None,
            command: "test.sh".to_string(),
            timeout: Some(30),
            status_message: None,
            async_hook: None,
            hook_type: "claude-command".to_string(),
        };
        assert!(hook1.is_supported_type());

        // "claude-command" should be valid
        let hook2 = HookConfig {
            event: "Stop".to_string(),
            matcher: None,
            command: "test.sh".to_string(),
            timeout: Some(30),
            status_message: None,
            async_hook: None,
            hook_type: "claude-command".to_string(),
        };
        assert!(hook2.is_supported_type());

        let hook_persistent = HookConfig {
            event: "Stop".to_string(),
            matcher: None,
            command: "test.sh".to_string(),
            timeout: Some(30),
            status_message: None,
            async_hook: None,
            hook_type: "claude-command-persistent".to_string(),
        };
        assert!(hook_persistent.is_supported_type());

        // Unknown type should be invalid
        let hook3 = HookConfig {
            event: "Stop".to_string(),
            matcher: None,
            command: "test.sh".to_string(),
            timeout: Some(30),
            status_message: None,
            async_hook: None,
            hook_type: "future-v2".to_string(),
        };
        assert!(!hook3.is_supported_type());
    }

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

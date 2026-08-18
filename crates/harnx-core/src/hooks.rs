use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

/// Environment variable describing the stdio protocol expected from a hook.
pub const HARNX_HOOK_PROTOCOL_ENV: &str = "HARNX_HOOK_PROTOCOL";

/// Value of [`HARNX_HOOK_PROTOCOL_ENV`] for persistent JSON Lines hooks.
pub const HARNX_HOOK_PROTOCOL_JSONL: &str = "jsonl";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookPayload {
    pub session_id: String,
    pub cwd: PathBuf,
    #[serde(default)]
    pub resume_count: u32,
    #[serde(flatten)]
    pub hook_event: HookEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "hook_event_name", rename_all = "PascalCase")]
pub enum HookEvent {
    SessionStart {
        source: String,
        model: String,
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
    InstructionsLoaded {
        file_path: PathBuf,
        memory_type: String,
        load_reason: String,
    },
    CwdChanged {
        old_cwd: PathBuf,
        new_cwd: PathBuf,
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
            Self::UserPromptSubmit { .. } => "UserPromptSubmit",
            Self::Stop { .. } => "Stop",
            Self::StopFailure { .. } => "StopFailure",
            Self::InstructionsLoaded { .. } => "InstructionsLoaded",
            Self::CwdChanged { .. } => "CwdChanged",
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HookResultControl {
    Continue,
    Block { reason: String },
    Ask { reason: Option<String> },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HookSpecificOutput {
    #[serde(default)]
    pub permission_decision: Option<String>,
    #[serde(default)]
    pub permission_decision_reason: Option<String>,
    #[serde(default)]
    pub tool_input: Option<Value>,
    #[serde(default)]
    pub tool_response: Option<Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    #[serde(default)]
    pub mutated_tool_input: Option<Value>,
    #[serde(default)]
    pub mutated_tool_response: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookOutcome {
    pub control: HookResultControl,
    pub result: HookResult,
}

// --- HookConfig / HooksConfig (serialized shape read from config.yaml) -------

/// Configuration for a single hook entry
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct HookConfig {
    /// Hook server command to execute
    pub command: String,

    /// Optional status message to display
    #[serde(default)]
    pub status_message: Option<String>,

    #[serde(default, rename = "async")]
    pub async_hook: Option<bool>,

    /// Directory of the package that owns this hook, set at load time when the
    /// hook was defined by a packaged MCP server. Injected into the hook
    /// process environment as `HARNX_PACKAGE_DIR` so bundled hook scripts can
    /// be referenced relative to their package. Not serialized. `None` for
    /// hooks not owned by a package (global or unpackaged config); those fall
    /// back to the config dir at spawn time.
    #[serde(skip)]
    pub package_dir: Option<PathBuf>,
}

/// Configuration for all hooks (global or per-agent)
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct HooksConfig {
    /// Maximum number of resume iterations
    #[serde(default)]
    pub max_resume: Option<u32>,

    /// List of hook entries
    #[serde(default)]
    pub entries: Vec<HookConfig>,
}

impl HooksConfig {
    /// Merge global and agent hooks.
    ///
    /// Agent entries follow global entries so declaration order remains the
    /// dispatch tiebreak within the merged supervisor.
    pub fn merge(global: &HooksConfig, agent: &HooksConfig) -> HooksConfig {
        let mut entries = global.entries.clone();
        entries.extend(agent.entries.iter().cloned());

        HooksConfig {
            max_resume: agent.max_resume.or(global.max_resume),
            entries,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        HookConfig, HookEvent, HookOutcome, HookPayload, HookResult, HookResultControl,
        HookSpecificOutput, HooksConfig,
    };
    use serde_json::{json, Value};
    use std::path::PathBuf;

    #[test]
    fn test_hooks_config_parse() {
        let yaml = r#"
max_resume: 3
entries:
  - command: "/path/to/hook-server --event Stop -- /path/to/hook.sh"
    async: true
"#;

        let config: HooksConfig = serde_yaml::from_str(yaml).expect("parse hooks config");

        assert_eq!(config.max_resume, Some(3));
        assert_eq!(config.entries.len(), 1);
        let entry = &config.entries[0];
        assert_eq!(
            entry.command,
            "/path/to/hook-server --event Stop -- /path/to/hook.sh"
        );
        assert!(entry.status_message.is_none());
        assert_eq!(entry.async_hook, Some(true));
        assert!(entry.package_dir.is_none());
    }

    #[test]
    fn test_hooks_config_merge_preserves_declaration_order() {
        let hook = |command: &str| HookConfig {
            command: command.to_string(),
            status_message: None,
            async_hook: None,
            package_dir: None,
        };
        let global = HooksConfig {
            max_resume: Some(5),
            entries: vec![hook("global-first"), hook("global-second")],
        };
        let agent = HooksConfig {
            max_resume: Some(3),
            entries: vec![hook("agent-first")],
        };

        let merged = HooksConfig::merge(&global, &agent);

        assert_eq!(merged.max_resume, Some(3));
        assert_eq!(
            merged
                .entries
                .iter()
                .map(|entry| entry.command.as_str())
                .collect::<Vec<_>>(),
            vec!["global-first", "global-second", "agent-first"]
        );
    }

    #[test]
    fn test_hooks_config_default() {
        let config = HooksConfig::default();

        assert!(config.max_resume.is_none());
        assert!(config.entries.is_empty());
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
    fn test_hook_payload_json_round_trip() {
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

        let json = serde_json::to_string(&payload).expect("serialize hook payload");
        let decoded: HookPayload = serde_json::from_str(&json).expect("deserialize hook payload");

        assert_eq!(decoded.session_id, payload.session_id);
        assert_eq!(decoded.cwd, payload.cwd);
        assert_eq!(decoded.resume_count, payload.resume_count);
        match decoded.hook_event {
            HookEvent::PreToolUse {
                tool_name,
                tool_input,
                tool_use_id,
            } => {
                assert_eq!(tool_name, "shell");
                assert_eq!(tool_input, json!({"command": "echo hi"}));
                assert_eq!(tool_use_id, "call-1");
            }
            event => panic!("expected PreToolUse, got {event:?}"),
        }
    }

    #[test]
    fn test_hook_outcome_json_round_trip() {
        let outcome = HookOutcome {
            control: HookResultControl::Block {
                reason: "dangerous command".to_string(),
            },
            result: HookResult {
                mutated_tool_input: Some(json!({"command": "echo safe"})),
                ..HookResult::default()
            },
        };

        let json = serde_json::to_string(&outcome).expect("serialize hook outcome");
        let decoded: HookOutcome = serde_json::from_str(&json).expect("deserialize hook outcome");

        assert_eq!(decoded.control, outcome.control);
        assert_eq!(
            decoded.result.mutated_tool_input,
            Some(json!({"command": "echo safe"}))
        );
    }

    #[test]
    fn test_hook_result_control_json_round_trip() {
        let controls = [
            HookResultControl::Continue,
            HookResultControl::Block {
                reason: "blocked".to_string(),
            },
            HookResultControl::Ask { reason: None },
        ];

        for control in controls {
            let json = serde_json::to_string(&control).expect("serialize hook result control");
            let decoded: HookResultControl =
                serde_json::from_str(&json).expect("deserialize hook result control");
            assert_eq!(decoded, control);
        }
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

    fn assert_event_names(events: Vec<(HookEvent, &str)>) {
        for (event, expected_name) in events {
            assert_eq!(event.event_name(), expected_name);
            let serialized = serde_json::to_value(&event).expect("serialize hook event");
            assert_eq!(serialized["hook_event_name"], expected_name);
            let decoded: HookEvent =
                serde_json::from_value(serialized).expect("deserialize hook event");
            assert_eq!(decoded.event_name(), expected_name);
        }
    }

    #[test]
    fn lifecycle_event_names_round_trip() {
        assert_event_names(vec![
            (
                HookEvent::SessionStart {
                    source: "cli".to_string(),
                    model: "claude".to_string(),
                },
                "SessionStart",
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
        ]);
    }

    #[test]
    fn file_and_tool_event_names_round_trip() {
        assert_event_names(vec![
            (
                HookEvent::InstructionsLoaded {
                    file_path: PathBuf::from("/tmp/CLAUDE.md"),
                    memory_type: "Project".to_string(),
                    load_reason: "session_start".to_string(),
                },
                "InstructionsLoaded",
            ),
            (
                HookEvent::CwdChanged {
                    old_cwd: PathBuf::from("/tmp/old"),
                    new_cwd: PathBuf::from("/tmp/new"),
                },
                "CwdChanged",
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
        ]);
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
        assert!(output.tool_input.is_none());
        assert!(output.tool_response.is_none());
    }

    #[test]
    fn test_hook_specific_output_deserializes_tool_input() {
        let output: HookSpecificOutput = serde_json::from_str(r#"{"toolInput":{"mutated":true}}"#)
            .expect("deserialize tool input mutation");

        assert_eq!(output.tool_input, Some(json!({"mutated": true})));
        assert!(output.tool_response.is_none());
    }

    #[test]
    fn test_hook_specific_output_deserializes_tool_response() {
        let output: HookSpecificOutput = serde_json::from_str(r#"{"toolResponse":{"ok":true}}"#)
            .expect("deserialize tool response mutation");

        assert!(output.tool_input.is_none());
        assert_eq!(output.tool_response, Some(json!({"ok": true})));
    }

    #[test]
    fn test_hook_specific_output_deserializes_both_mutations() {
        let output: HookSpecificOutput =
            serde_json::from_str(r#"{"toolInput":{"mutated":true},"toolResponse":{"ok":true}}"#)
                .expect("deserialize both hook mutations");

        assert_eq!(output.tool_input, Some(json!({"mutated": true})));
        assert_eq!(output.tool_response, Some(json!({"ok": true})));
    }

    #[test]
    fn test_hook_specific_output_partial() {
        let output: HookSpecificOutput = serde_json::from_str(r#"{"permissionDecision":"ask"}"#)
            .expect("deserialize partial hook specific output");

        assert_eq!(output.permission_decision.as_deref(), Some("ask"));
        assert!(output.permission_decision_reason.is_none());
        assert!(output.tool_input.is_none());
        assert!(output.tool_response.is_none());
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
        assert!(result.mutated_tool_input.is_none());
        assert!(result.mutated_tool_response.is_none());
    }

    #[test]
    fn test_hook_result_deserializes_mutated_tool_fields() {
        let result: HookResult = serde_json::from_str(
            r#"{"mutatedToolInput":{"mutated":true},"mutatedToolResponse":{"ok":true}}"#,
        )
        .expect("deserialize mutated tool fields");

        assert_eq!(result.mutated_tool_input, Some(json!({"mutated": true})));
        assert_eq!(result.mutated_tool_response, Some(json!({"ok": true})));
    }
}

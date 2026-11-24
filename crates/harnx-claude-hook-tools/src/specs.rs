use harnx_toolset::{CancellationGuarantee, ToolProgressKind, ToolSpec};
use serde_json::json;

pub(crate) fn run() -> ToolSpec {
    ToolSpec {
        name: "run".into(),
        description: "Run a trusted Claude-protocol hook command. Returns decision (allow/deny/ask), reason, tool_input/tool_response replacements, context strings and raw result. Does not enforce the decision: middleware must apply it. Arbitrary host execution; grant only to trusted middleware.".into(),
        input_schema: json!({
            "type":"object", "additionalProperties":false,
            "required":["command","event","payload"],
            "properties":{
                "command":{"type":"array","minItems":1,"items":{"type":"string"},"description":"Program and arguments, executed directly without a shell."},
                "persistent":{"type":"boolean","default":false,"description":"Reuse a resident HARNX_HOOK_PROTOCOL=jsonl process. It must echo each request id in its response."},
                "event":{"type":"string","enum":["PreToolUse","PostToolUse","PostToolUseFailure","SessionStart","UserPromptSubmit","Stop","StopFailure","InstructionsLoaded","CwdChanged"]},
                "payload":{"type":"object","additionalProperties":true,"description":"Claude event fields. hook_event_name is added from event; id is reserved. cwd, if set, must be an absolute directory on this tool server."},
                "timeout_secs":{"type":"integer","minimum":1,"maximum":300,"default":30,"description":"Total deadline including waiting for a resident process, writing stdin and reading output."},
                "package_dir":{"type":"string","description":"Optional HARNX_PACKAGE_DIR for package-owned commands."}
            }
        }),
        cancellation_guarantee: CancellationGuarantee::Cooperative,
        idempotent_hint: false,
        read_only_hint: false,
        // The handler owns its deadline; middleware also has its inclusive limit.
        timeout_secs: Some(0),
        meta: None,
    }.with_kind(ToolProgressKind::Execute)
     .with_call_template("Claude hook {{ event }}")
}

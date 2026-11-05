//! Synthesized `bash` ToolDeclaration for script mode (#2537).
//!
//! When an agent's `bash_tools` resolves to a non-empty inner tool set, the
//! model gets one extra `bash` tool whose arguments mirror `bash_exec`. The
//! script crate generates its dialect guide from the backend and inner schemas.

use harnx_core::tool::{JsonSchema, ToolDeclaration};

/// The tool name agents see when `bash_tools` resolves to a non-empty set.
/// Distinct from `bash_exec`, which shells out to a real host shell.
pub const BASH_TOOL_NAME: &str = "bash";

/// Build the `bash` declaration for a resolved inner tool set. The set must
/// already exclude control tools (see `tool_selection::select_bash_inner_tools`).
pub(crate) fn synthesize_bash_declaration(inner_tools: &[ToolDeclaration]) -> ToolDeclaration {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "script": {
                "type": "string",
                "description": "Bash script to run. The shell commands available to the script are the tools listed in this tool's description."
            },
            "timeout_secs": {
                "type": "integer",
                "default": harnx_script::ScriptLimits::default().timeout_secs,
                "description": "Kill the script after this many seconds. 0 means no deadline."
            },
            "head_lines": {
                "type": "integer",
                "description": "Return only the first N lines of the script output."
            },
            "tail_lines": {
                "type": "integer",
                "description": "Return only the last N lines of the script output (combined with head_lines shows both ends)."
            },
            "max_output_bytes": {
                "type": "integer",
                "description": "Truncate the script output to this many bytes."
            }
        },
        "required": ["script"]
    });
    ToolDeclaration {
        name: BASH_TOOL_NAME.to_string(),
        description: bash_tool_description(inner_tools),
        parameters: serde_json::from_value::<JsonSchema>(schema)
            .expect("bash tool schema is valid"),
        mcp_tool_name: None,
        mcp_server_name: None,
        call_template: None,
        result_template: None,
        // A script fans out into multiple side-effecting tool calls; on worker
        // loss it is reported interrupted and never rerun (plan note
        // "decisions", worker-loss section), so both hints must stay false.
        idempotent_hint: Some(false),
        read_only_hint: Some(false),
        // Execution-kind like the shell tool it replaces (`bash_exec`); used
        // by progress rendering and UI grouping.
        kind: Some(harnx_core::event::ToolKind::Execute),
    }
}

/// Use the same schema conversion as BashToolProvider's executable catalog.
fn bash_tool_description(inner_tools: &[ToolDeclaration]) -> String {
    let specs: Vec<_> = inner_tools
        .iter()
        .map(|tool| harnx_script::commands::ToolSpec {
            name: tool.name.clone(),
            description: tool.description.clone(),
            schema: tool.parameters.as_value().clone(),
        })
        .collect();
    harnx_script::description::bash_tool_description(
        &specs,
        crate::bash_inner::DEFAULT_MAX_CONCURRENT_INNER_CALLS,
        crate::bash_inner::DEFAULT_MAX_INNER_CALLS_PER_SCRIPT,
    )
}

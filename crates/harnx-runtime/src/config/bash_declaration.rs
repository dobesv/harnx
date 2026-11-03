//! Synthesized `bash` ToolDeclaration for script mode (#2537).
//!
//! When an agent's `bash_tools` resolves to a non-empty inner tool set, the
//! model gets one extra `bash` tool whose arguments mirror `bash_exec`. The
//! description here is a placeholder: it lists each inner tool's name and a
//! one-line summary. The full dialect guide (planned:
//! crates/harnx-script/src/description.rs) replaces this generator, so keep
//! all wording inside this module so the swap is a single-file change.

use harnx_core::tool::{JsonSchema, ToolDeclaration};

/// The tool name agents see when `bash_tools` resolves to a non-empty set.
/// Distinct from `bash_exec`, which shells out to a real host shell.
pub const BASH_TOOL_NAME: &str = "bash";

/// Default wall-time budget for a script, mirroring `bash_exec` (24 hours).
const DEFAULT_TIMEOUT_SECS: u64 = 86_400;

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
                "default": DEFAULT_TIMEOUT_SECS,
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

/// Placeholder description: purpose line plus one bullet per inner tool.
/// The full dialect guide (crates/harnx-script/src/description.rs) replaces
/// this once the script backend lands.
fn bash_tool_description(inner_tools: &[ToolDeclaration]) -> String {
    let mut description = String::from(
        "Run a bash script whose commands are this agent's tools, chaining several tool calls \
and post-processing their results in one round trip. Placeholder description; a later \
release documents the shell dialect and limits. Tool commands available in the script:\n",
    );
    for tool in inner_tools {
        description.push_str(&format!(
            "- {}: {}\n",
            tool.name,
            one_line_summary(&tool.description)
        ));
    }
    description
}

/// First line of a tool description, whitespace-collapsed, as the one-line
/// summary shown next to the tool's command name.
fn one_line_summary(description: &str) -> String {
    description
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

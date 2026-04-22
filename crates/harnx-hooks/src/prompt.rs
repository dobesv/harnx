use inquire::Confirm;
use serde_json::Value;

/// Shows a confirmation prompt for tool calls that require user approval.
/// Returns `true` if the user approves, `false` if denied.
/// In non-interactive mode (no terminal), automatically denies.
pub fn confirm_tool_use(tool_name: &str, tool_input: &Value, reason: Option<&str>) -> bool {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        return false;
    }

    let input_preview = format_input_preview(tool_input);
    let mut message = format!("Hook requires confirmation for tool '{tool_name}'");
    if let Some(r) = reason {
        message.push_str(&format!("\nReason: {r}"));
    }
    if !input_preview.is_empty() {
        message.push_str(&format!("\nInput: {input_preview}"));
    }
    message.push_str("\nAllow this tool call?");

    Confirm::new(&message)
        .with_default(false)
        .prompt()
        .unwrap_or_default()
}

fn format_input_preview(input: &Value) -> String {
    let s = serde_json::to_string_pretty(input).unwrap_or_default();
    if s.len() > 500 {
        let truncated: String = s.chars().take(500).collect();
        format!("{truncated}...")
    } else {
        s
    }
}

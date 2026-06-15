//! Builds the session-compaction request. Compaction renders the older prefix
//! of the conversation into a single flattened-text transcript (keeping base64
//! attachments out of the request) and sends it to a summarizer; the most
//! recent turns are kept verbatim. See `session_ops_split::compact_session`.

use harnx_core::session::ToolOutput;

/// Number of recent user-turns kept verbatim (not summarized) by default.
pub const KEEP_RECENT_TURNS: usize = 3;
/// Token budget for the verbatim recent suffix (estimated tokens).
pub const KEEP_RECENT_TOKENS: usize = 8000;
/// Per-tool-result character cap when rendering tool output into the transcript.
pub const TOOL_OUTPUT_MAX_CHARS: usize = 2000;

/// Default summarizer system prompt, used when no `compaction_agent` is set.
pub const DEFAULT_COMPACT_SYSTEM_PROMPT: &str = "\
You are compacting a conversation transcript so it can continue within a smaller \
context window. Write a concise summary (about 200 words) that preserves: the \
user's request and intent, key decisions and facts established, files or commands \
examined or changed, errors encountered and how they were resolved, and any \
pending or next steps. Write plainly; do not invent details not present in the \
transcript.";

/// Truncate `text` to at most `max_chars`, keeping the head and tail and
/// inserting an elision marker in the middle. Returns the input unchanged when
/// it already fits. `max_chars` below the marker length returns a head slice.
pub fn truncate_middle(text: &str, max_chars: usize) -> String {
    let len = text.chars().count();
    if len <= max_chars {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let marker = "\n…[truncated]…\n";
    if max_chars <= marker.chars().count() {
        return chars.iter().take(max_chars).collect();
    }
    let budget = max_chars - marker.chars().count();
    let head = budget / 2;
    let tail = budget - head;
    let head_s: String = chars[..head].iter().collect();
    let tail_s: String = chars[len - tail..].iter().collect();
    format!("{head_s}{marker}{tail_s}")
}

/// Render one tool result into compact transcript text using a three-tier
/// fallback: (1) an explicit compaction string the tool supplied at
/// `output._meta["harnx.dev/compaction"]`; (2) the user-audience summary the
/// tool already emits; (3) the redacted output JSON, truncated head/tail.
pub fn render_tool_output(out: &ToolOutput, max_chars: usize) -> String {
    // Tier 1: explicit compaction block (MCP `_meta` extension).
    if let Some(text) = out
        .output
        .get("_meta")
        .and_then(|m| m.get("harnx.dev/compaction"))
        .and_then(|v| v.as_str())
    {
        return text.to_string();
    }
    // Tier 2: existing user-audience summary.
    if let Some(text) = harnx_core::tool::extract_user_display_text(&out.output) {
        return truncate_middle(&text, max_chars);
    }
    // Tier 3: redacted output JSON, truncated.
    let full = serde_json::to_string(&out.output).unwrap_or_default();
    truncate_middle(&full, max_chars)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_middle_keeps_short_text() {
        assert_eq!(truncate_middle("hello", 100), "hello");
    }

    #[test]
    fn truncate_middle_elides_long_text() {
        let text = "a".repeat(500);
        let out = truncate_middle(&text, 100);
        assert!(out.chars().count() <= 100);
        assert!(out.contains("[truncated]"));
        assert!(out.starts_with('a'));
        assert!(out.ends_with('a'));
    }

    #[test]
    fn tool_result_tier1_uses_explicit_compaction_meta() {
        use harnx_core::session::ToolOutput;
        use serde_json::json;
        let out = ToolOutput {
            id: None,
            name: "bash".into(),
            output: json!({
                "content": [{"type": "text", "text": "long full output..."}],
                "_meta": {"harnx.dev/compaction": "exit 0; built ok"}
            }),
            content: vec![],
            switch_agent: None,
        };
        assert_eq!(render_tool_output(&out, 2000), "exit 0; built ok");
    }

    #[test]
    fn tool_result_tier2_uses_user_audience_summary() {
        use harnx_core::session::ToolOutput;
        use serde_json::json;
        let out = ToolOutput {
            id: None,
            name: "fs".into(),
            output: json!({
                "content": [
                    {"type": "text", "text": "FULL", "annotations": {"audience": ["assistant"]}},
                    {"type": "text", "text": "wrote 3 files", "annotations": {"audience": ["user"]}}
                ]
            }),
            content: vec![],
            switch_agent: None,
        };
        assert_eq!(render_tool_output(&out, 2000), "wrote 3 files");
    }

    #[test]
    fn tool_result_tier3_truncates_full_output() {
        use harnx_core::session::ToolOutput;
        use serde_json::json;
        let big = "x".repeat(5000);
        let out = ToolOutput {
            id: None,
            name: "bash".into(),
            output: json!({ "data": big }),
            content: vec![],
            switch_agent: None,
        };
        let rendered = render_tool_output(&out, 200);
        assert!(rendered.chars().count() <= 200);
        assert!(rendered.contains("[truncated]"));
    }
}

//! Builds the session-compaction request. Compaction renders the older prefix
//! of the conversation into a single flattened-text transcript (keeping base64
//! attachments out of the request) and sends it to a summarizer; the most
//! recent turns are kept verbatim. See `session_ops_split::compact_session`.

use harnx_core::message::{
    Message, MessageContent, MessageContentPart, MessageContentToolCalls, MessageRole,
};
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

/// Render image/text content parts to plain text, replacing images with a
/// `[image: <ref>]` placeholder so no base64 enters the transcript.
fn render_parts(parts: &[MessageContentPart]) -> String {
    let mut out = Vec::new();
    for part in parts {
        match part {
            MessageContentPart::Text { text } => out.push(text.clone()),
            MessageContentPart::ImageUrl { image_url } => {
                out.push(format!("[image: {}]", image_url.url))
            }
        }
    }
    out.join("\n")
}

/// Render a tool-call turn: the assistant's preceding text, each tool call
/// header, and each tool result via the three-tier renderer.
fn render_tool_calls(calls: &MessageContentToolCalls, max_tool_chars: usize) -> String {
    let mut lines: Vec<String> = Vec::new();
    if !calls.text.is_empty() {
        lines.push(format!("── assistant ──\n{}", calls.text));
    }
    for result in &calls.tool_results {
        let args = serde_json::to_string(&result.call.arguments).unwrap_or_default();
        lines.push(format!("── tool call: {} ──\n{}", result.call.name, args));
        let out = ToolOutput {
            id: result.call.id.clone(),
            name: result.call.name.clone(),
            output: result.output.clone(),
            content: result.content.clone(),
            switch_agent: result.switch_agent.clone(),
        };
        lines.push(format!(
            "── tool result ──\n{}",
            render_tool_output(&out, max_tool_chars)
        ));
    }
    lines.join("\n\n")
}

/// Render a slice of messages into a single flattened, labeled transcript for
/// summarization. Images are placeholdered; tool results use the three-tier
/// `render_tool_output`. `max_tool_chars` caps each tool result.
pub fn render_transcript(messages: &[Message], max_tool_chars: usize) -> String {
    let mut sections: Vec<String> = Vec::new();
    for message in messages {
        match (&message.role, &message.content) {
            (MessageRole::System, content) => {
                sections.push(format!("── system ──\n{}", content.to_text()));
            }
            (MessageRole::User, MessageContent::Array(parts)) => {
                sections.push(format!("── user ──\n{}", render_parts(parts)));
            }
            (MessageRole::User, content) => {
                sections.push(format!("── user ──\n{}", content.to_text()));
            }
            (MessageRole::Assistant, content) => {
                sections.push(format!("── assistant ──\n{}", content.to_text()));
            }
            (MessageRole::Tool, MessageContent::ToolCalls(calls)) => {
                sections.push(render_tool_calls(calls, max_tool_chars));
            }
            (MessageRole::Tool, content) => {
                sections.push(format!("── tool result ──\n{}", content.to_text()));
            }
        }
    }
    sections.join("\n\n")
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

    #[test]
    fn render_transcript_labels_roles_and_placeholders_images() {
        use harnx_core::message::{
            ImageUrl, Message, MessageContent, MessageContentPart, MessageContentToolCalls,
            MessageRole,
        };
        use harnx_core::tool::{ToolCall, ToolResult};
        use serde_json::json;

        let messages = vec![
            Message::new(
                MessageRole::System,
                MessageContent::Text("you are X".into()),
            ),
            Message::new(
                MessageRole::User,
                MessageContent::Array(vec![
                    MessageContentPart::Text {
                        text: "look at this".into(),
                    },
                    MessageContentPart::ImageUrl {
                        image_url: ImageUrl {
                            url: "cid:abc123".into(),
                        },
                    },
                ]),
            ),
            Message::new(
                MessageRole::Tool,
                MessageContent::ToolCalls(MessageContentToolCalls {
                    text: "let me check".into(),
                    thought: None,
                    sequence: false,
                    tool_results: vec![ToolResult {
                        call: ToolCall {
                            name: "bash".into(),
                            arguments: json!({"cmd": "ls"}),
                            id: Some("c1".into()),
                            thought_signature: None,
                        },
                        output: json!({"_meta": {"harnx.dev/compaction": "3 files"}}),
                        content: vec![],
                        switch_agent: None,
                    }],
                }),
            ),
        ];

        let t = render_transcript(&messages, 2000);
        assert!(t.contains("── system ──"));
        assert!(t.contains("you are X"));
        assert!(t.contains("── user ──"));
        assert!(t.contains("look at this"));
        assert!(
            t.contains("[image:"),
            "image rendered as placeholder, not base64"
        );
        assert!(t.contains("── tool call: bash ──"));
        assert!(t.contains("── tool result ──"));
        assert!(t.contains("3 files"));
    }
}

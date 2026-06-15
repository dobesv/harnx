//! Builds the session-compaction request. Compaction renders the older prefix
//! of the conversation into a single flattened-text transcript (keeping base64
//! attachments out of the request) and sends it to a summarizer; the most
//! recent turns are kept verbatim. See `session_ops_split::compact_session`.

use harnx_core::agent_config::AgentConfig;
use harnx_core::message::{
    Message, MessageContent, MessageContentPart, MessageContentToolCalls, MessageRole,
};
use harnx_core::model::Model;
use harnx_core::session::ToolOutput;

/// Number of recent user-turns kept verbatim (not summarized) by default.
pub const DEFAULT_KEEP_RECENT_TURNS: usize = 3;
/// Token budget for the verbatim recent suffix (estimated tokens).
pub const DEFAULT_KEEP_RECENT_TOKENS: usize = 8000;
/// Per-tool-result character cap when rendering tool output into the transcript.
pub const DEFAULT_TOOL_OUTPUT_MAX_CHARS: usize = 2000;

/// Resolved compaction tuning values for a single compaction run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionParams {
    pub keep_recent_turns: usize,
    pub keep_recent_tokens: usize,
    pub tool_output_max_chars: usize,
}

/// Resolve the compaction tuning values from the compaction agent's config,
/// falling back to the built-in defaults for any field the agent does not set.
/// The compaction agent is the configured `compaction_agent`, or the synthetic
/// default summarizer when none is configured — in which case all three are
/// unset and the defaults apply.
pub fn compaction_params(agent: &AgentConfig) -> CompactionParams {
    CompactionParams {
        keep_recent_turns: agent
            .compaction_keep_recent_turns()
            .unwrap_or(DEFAULT_KEEP_RECENT_TURNS),
        keep_recent_tokens: agent
            .compaction_keep_recent_tokens()
            .unwrap_or(DEFAULT_KEEP_RECENT_TOKENS),
        tool_output_max_chars: agent
            .compaction_tool_output_max_chars()
            .unwrap_or(DEFAULT_TOOL_OUTPUT_MAX_CHARS),
    }
}

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

/// Earliest index still within both the turn-count and token budgets, scanning
/// backward from the end.
fn recent_suffix_floor(
    messages: &[Message],
    model: &Model,
    keep_turns: usize,
    keep_tokens: usize,
) -> usize {
    let len = messages.len();
    let mut idx = len;
    let mut turns = 0usize;
    let mut tokens = 0usize;
    for i in (0..len).rev() {
        let t = model.messages_tokens(std::slice::from_ref(&messages[i]));
        let is_user = messages[i].role == MessageRole::User;
        let over_turn_budget = is_user && turns >= keep_turns;
        let over_token_budget = tokens + t > keep_tokens;
        if over_turn_budget || over_token_budget {
            break;
        }
        tokens += t;
        if is_user {
            turns += 1;
        }
        idx = i;
    }
    idx
}

/// Index in `messages` where the verbatim recent suffix begins; messages before
/// it are compacted. Keeps at most `keep_turns` recent user-turns and
/// `keep_tokens` estimated tokens, snapping the boundary forward to a `User`
/// message. When `len >= 2` and the slice contains a user message — which the
/// compaction caller guarantees by checking `has_user_messages` first — at
/// least the first message is compacted. (A multi-message slice with no user
/// message at all returns `len`, i.e. compacts nothing.)
pub fn split_index(
    messages: &[Message],
    model: &Model,
    keep_turns: usize,
    keep_tokens: usize,
) -> usize {
    let len = messages.len();
    if len <= 1 {
        return len;
    }
    let mut idx = recent_suffix_floor(messages, model, keep_turns, keep_tokens);
    // Snap forward to a User boundary so a kept reply always has its prompt.
    while idx < len && messages[idx].role != MessageRole::User {
        idx += 1;
    }
    // Never compact zero messages when there is something to compact: if the
    // budget would keep everything, fall back to splitting at the last user
    // turn so at least the prefix before it is compacted. When the only user
    // message is at index 0 there is no earlier turn to keep, so we compact
    // everything (idx = len).
    if idx == 0 {
        idx = messages
            .iter()
            .rposition(|m| m.role == MessageRole::User)
            .filter(|&p| p >= 1)
            .unwrap_or(len);
    }
    idx.min(len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compaction_params_uses_agent_overrides_then_defaults() {
        use harnx_core::agent_config::AgentConfig;

        // No overrides → defaults.
        let default_agent = AgentConfig::from_prompt("summarize");
        let p = compaction_params(&default_agent);
        assert_eq!(p.keep_recent_turns, DEFAULT_KEEP_RECENT_TURNS);
        assert_eq!(p.keep_recent_tokens, DEFAULT_KEEP_RECENT_TOKENS);
        assert_eq!(p.tool_output_max_chars, DEFAULT_TOOL_OUTPUT_MAX_CHARS);

        // Agent override wins (per field); unset field still falls back.
        let md = "---\n\
compaction_keep_recent_turns: 1\n\
compaction_tool_output_max_chars: 50\n\
---\n\
summarize\n";
        let agent = AgentConfig::from_markdown("c", md).unwrap();
        let p = compaction_params(&agent);
        assert_eq!(p.keep_recent_turns, 1);
        assert_eq!(p.tool_output_max_chars, 50);
        assert_eq!(p.keep_recent_tokens, DEFAULT_KEEP_RECENT_TOKENS);
    }

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

    fn msg(role: harnx_core::message::MessageRole, text: &str) -> harnx_core::message::Message {
        use harnx_core::message::{Message, MessageContent};
        Message::new(role, MessageContent::Text(text.into()))
    }

    #[test]
    fn split_index_keeps_recent_user_turns() {
        use harnx_core::message::MessageRole::*;
        use harnx_core::model::Model;
        let model = Model::default();
        let messages = vec![
            msg(System, "sys"),
            msg(User, "u1"),
            msg(Assistant, "a1"),
            msg(User, "u2"),
            msg(Assistant, "a2"),
            msg(User, "u3"),
            msg(Assistant, "a3"),
        ];
        let idx = split_index(&messages, &model, 1, 100_000);
        assert_eq!(idx, 5);
        assert_eq!(messages[idx].role, User);
    }

    #[test]
    fn split_index_compacts_at_least_one_message() {
        use harnx_core::message::MessageRole::*;
        use harnx_core::model::Model;
        let model = Model::default();
        let messages = vec![msg(User, "u1"), msg(Assistant, "a1")];
        let idx = split_index(&messages, &model, 100, 100_000);
        assert!(idx >= 1 && idx <= messages.len());
    }

    #[test]
    fn split_index_single_message_keeps_all() {
        use harnx_core::message::MessageRole::*;
        use harnx_core::model::Model;
        let model = Model::default();
        let messages = vec![msg(User, "only")];
        assert_eq!(split_index(&messages, &model, 3, 8000), messages.len());
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

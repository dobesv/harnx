//! Builds the session-compaction request. Compaction renders dropped messages
//! into a sanitized, budgeted transcript for the summarizer. Recent turns, or
//! the latest prompt plus a round-safe tail, are kept verbatim.
//! See `session_ops_compaction::Config::compact_session`.

use harnx_core::agent_config::AgentConfig;
use harnx_core::message::{Message, MessageContent, MessageRole};
use harnx_core::model::Model;
use harnx_core::session::ToolOutput;

mod completion;
mod media;
#[cfg(test)]
mod media_tests;
mod request;
mod transcript;
#[cfg(test)]
mod transcript_tests;
pub(crate) use request::fetch_summary;
#[cfg(test)]
use transcript::render_transcript;
use transcript::strip_data_urls;
pub(crate) use transcript::Transcript;

/// Number of recent user-turns kept verbatim (not summarized) by default.
///
/// Timing notices are not counted as turns: they are non-executable context
/// preserved during compaction analysis but excluded from `keep_recent_turns`
/// calculations and split-point decisions.
pub const DEFAULT_KEEP_RECENT_TURNS: usize = 3;
/// Token budget for the verbatim recent suffix (estimated tokens).
pub const DEFAULT_KEEP_RECENT_TOKENS: usize = 8000;
/// Per-tool-result character cap when rendering tool output into the transcript.
pub const DEFAULT_TOOL_OUTPUT_MAX_CHARS: usize = 2000;

/// Resolved compaction tuning values for a single compaction run.
/// Setting `keep_recent_turns` to zero disables verbatim retention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionParams {
    pub keep_recent_turns: usize,
    pub keep_recent_tokens: usize,
    pub tool_output_max_chars: usize,
}

/// Ordered kept set: an optional latest-turn prompt followed by a suffix.
/// Invariant: `prompt_idx < tail_from` when `prompt_idx` is present. `tail_from == len`
/// means no tail; `None` plus `len` means everything is summarized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeepPlan {
    pub prompt_idx: Option<usize>,
    pub tail_from: usize,
}

impl KeepPlan {
    /// Whether an in-bounds message index belongs to the kept set.
    pub fn contains(&self, index: usize) -> bool {
        index >= self.tail_from || self.prompt_idx == Some(index)
    }

    pub fn kept_count(&self, len: usize) -> usize {
        len.saturating_sub(self.tail_from)
            + usize::from(self.prompt_idx.is_some_and(|i| i < self.tail_from.min(len)))
    }

    pub fn kept_indices(&self, len: usize) -> Vec<usize> {
        self.prompt_idx
            .filter(|&i| i < self.tail_from.min(len))
            .into_iter()
            .chain(self.tail_from.min(len)..len)
            .collect()
    }
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
transcript. In every section, keep what the user said and what the agent replied that bears on it. Quote short requests, questions, and statements directly, and summarize long ones, keeping their results, links, numbers, warnings, and recommendations. If the latest user prompt isn't kept verbatim after the summary, quote it. Give the most recent requests and replies the most weight: someone reading only the summary should be able to tell what the user last asked and what the agent last answered.";

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
        return strip_data_urls(text).into_owned();
    }
    // Tier 2: existing user-audience summary.
    if let Some(text) = harnx_core::tool::extract_user_display_text(&out.output) {
        return truncate_middle(&strip_data_urls(&text), max_chars);
    }
    // Tier 3 also handles raw media from audio and lossless tool responses.
    let full = media::sanitized_json(&out.output);
    truncate_middle(&strip_data_urls(&full), max_chars)
}

pub(crate) fn is_turn_prompt(message: &Message) -> bool {
    if !message.role.is_user() {
        return false;
    }
    let text = match &message.content {
        MessageContent::Text(text) => std::borrow::Cow::Borrowed(text.as_str()),
        _ => std::borrow::Cow::Owned(message.content.to_text()),
    };
    !harnx_core::timing_identity::is_timing_notice(&text)
}

/// ToolCalls stores the assistant invocation and all results in one message.
/// An assistant reply is its own boundary, so an oversized last tool round can
/// be summarized without also dropping the final reply.
fn is_round_boundary(message: &Message) -> bool {
    message.role == MessageRole::Assistant
        || matches!(message.content, MessageContent::ToolCalls(_))
}

/// Keep whole recent turns when they fit; otherwise keep the latest prompt and
/// a round-safe suffix of that turn. Always summarize at least one message.
/// A transcript consisting only of a prompt must summarize that prompt.
pub fn plan_compaction(
    messages: &[Message],
    model: &Model,
    params: &CompactionParams,
    effective_threshold: usize,
) -> KeepPlan {
    if params.keep_recent_turns == 0 {
        return KeepPlan {
            prompt_idx: None,
            tail_from: messages.len(),
        };
    }
    let tail_from = whole_turn_suffix(messages, model, params);
    if tail_from < messages.len() {
        return KeepPlan {
            prompt_idx: None,
            tail_from,
        };
    }
    split_latest_turn(
        messages,
        model,
        params.keep_recent_tokens,
        effective_threshold,
    )
    .leave_something_to_compact(messages)
}

fn whole_turn_suffix(messages: &[Message], model: &Model, params: &CompactionParams) -> usize {
    let floor = recent_suffix_floor(
        messages,
        model,
        params.keep_recent_turns,
        params.keep_recent_tokens,
    );
    let tail_from = (floor..messages.len())
        .find(|&i| is_turn_prompt(&messages[i]))
        .unwrap_or(messages.len());
    if tail_from == 0 {
        // A forced compaction still needs input when the whole session fits.
        return messages
            .iter()
            .rposition(is_turn_prompt)
            .filter(|&i| i > 0)
            .unwrap_or(messages.len());
    }
    tail_from
}

fn split_latest_turn(
    messages: &[Message],
    model: &Model,
    keep_tokens: usize,
    effective_threshold: usize,
) -> KeepPlan {
    let latest_prompt = messages.iter().rposition(is_turn_prompt);
    let prompt_tokens = latest_prompt
        .map(|i| model.messages_tokens(&messages[i..=i]))
        .unwrap_or(0);
    let prompt_idx = latest_prompt.filter(|_| {
        prompt_tokens <= keep_tokens || prompt_tokens < effective_threshold.div_ceil(2)
    });
    let reserved = prompt_idx.map_or(0, |_| prompt_tokens);
    let tail_from = round_safe_tail(
        messages,
        model,
        latest_prompt.map_or(0, |i| i + 1),
        keep_tokens.saturating_sub(reserved),
    );
    KeepPlan {
        prompt_idx,
        tail_from,
    }
}

fn round_safe_tail(
    messages: &[Message],
    model: &Model,
    after_prompt: usize,
    budget: usize,
) -> usize {
    let mut tail_from = messages.len();
    let mut tokens = 0usize;
    for i in (after_prompt..messages.len()).rev() {
        tokens = tokens.saturating_add(model.messages_tokens(&messages[i..=i]));
        if tokens > budget {
            break;
        }
        if is_round_boundary(&messages[i]) {
            tail_from = i;
        }
    }
    tail_from
}

impl KeepPlan {
    /// A failed request's prepared input must survive recovery verbatim, even
    /// when the ordinary keep budget would summarize it. Other splits stay C2's.
    pub(crate) fn protect_suffix(mut self, protected_from: Option<usize>) -> Self {
        if let Some(from) = protected_from {
            self.tail_from = self.tail_from.min(from);
            self.prompt_idx = self.prompt_idx.filter(|&i| i < self.tail_from);
        }
        self
    }

    fn leave_something_to_compact(mut self, messages: &[Message]) -> Self {
        if messages.is_empty() || self.kept_count(messages.len()) < messages.len() {
            return self;
        }
        // Drop the oldest post-prompt round, or summarize a lone prompt.
        if self.tail_from == messages.len() {
            self.prompt_idx = None;
            return self;
        }
        self.tail_from = (self.tail_from + 1..messages.len())
            .find(|&i| is_round_boundary(&messages[i]))
            .unwrap_or(messages.len());
        self
    }
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
        let is_user = is_turn_prompt(&messages[i]);
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

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::message::MessageContentToolCalls;
    use serde_json::json;

    fn msg(role: MessageRole, text: &str) -> Message {
        Message::new(role, MessageContent::Text(text.into()))
    }

    #[test]
    fn default_prompt_preserves_latest_request_guard_and_reply_instruction() {
        assert!(DEFAULT_COMPACT_SYSTEM_PROMPT
            .contains("If the latest user prompt isn't kept verbatim after the summary, quote it"));
        assert!(DEFAULT_COMPACT_SYSTEM_PROMPT.contains(
            "someone reading only the summary should be able to tell what the user last asked and what the agent last answered"
        ));
    }

    #[test]
    fn turn_prompt_classification_preserves_text_and_multipart_timing_semantics() {
        use harnx_core::message::{ImageUrl, MessageContentPart};
        use harnx_core::timing_identity::{TimingReceipt, TIMING_HEADER};

        let receipt = TimingReceipt::initial(chrono::Utc::now(), "turn".into());
        let notice = format!("{TIMING_HEADER}{}", receipt.to_json());
        let (head, tail) = notice.split_once(',').unwrap();
        let text = |text: String| MessageContentPart::Text { text };
        let contents = [
            (MessageContent::Text("prompt".into()), true),
            (MessageContent::Text(notice.clone()), false),
            (MessageContent::Text(TIMING_HEADER.into()), true),
            (MessageContent::Array(vec![text(notice.clone())]), false),
            (
                MessageContent::Array(vec![text(format!("{head},")), text(tail.into())]),
                false,
            ),
            (
                MessageContent::Array(vec![text(notice.clone()), text("not JSON".into())]),
                true,
            ),
            (
                MessageContent::Array(vec![text(format!("{TIMING_HEADER}{{")), text("}".into())]),
                true,
            ),
            (
                MessageContent::Array(vec![
                    text(format!("{notice} | context")),
                    text("more".into()),
                ]),
                false,
            ),
            (
                MessageContent::Array(vec![MessageContentPart::ImageUrl {
                    image_url: ImageUrl {
                        url: "cid:image".into(),
                    },
                }]),
                true,
            ),
        ];
        for (content, expected) in contents {
            for role in [
                MessageRole::User,
                MessageRole::Assistant,
                MessageRole::System,
                MessageRole::Tool,
            ] {
                let message = Message::new(role, content.clone());
                assert_eq!(
                    is_turn_prompt(&message),
                    role.is_user() && expected,
                    "{message:?}"
                );
            }
        }
    }

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
---\n\nsummarize\n";
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
            markdown: None,
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
            markdown: None,
            content: vec![],
            switch_agent: None,
        };
        assert_eq!(render_tool_output(&out, 2000), "wrote 3 files");
    }

    #[test]
    fn tool_result_tier3_truncates_json() {
        use harnx_core::session::ToolOutput;
        use serde_json::json;
        let out = ToolOutput {
            id: None,
            name: "bash".into(),
            output: json!({
                "stdout": "a".repeat(5000),
                "stderr": "",
                "exit_code": 0
            }),
            markdown: None,
            content: vec![],
            switch_agent: None,
        };
        let rendered = render_tool_output(&out, 100);
        assert!(rendered.contains("[truncated]"));
    }

    fn plan(messages: &[Message], turns: usize, tokens: usize) -> KeepPlan {
        plan_with_threshold(messages, turns, tokens, 180_000)
    }

    fn plan_with_threshold(
        messages: &[Message],
        turns: usize,
        tokens: usize,
        threshold: usize,
    ) -> KeepPlan {
        let params = CompactionParams {
            keep_recent_turns: turns,
            keep_recent_tokens: tokens,
            tool_output_max_chars: DEFAULT_TOOL_OUTPUT_MAX_CHARS,
        };
        plan_compaction(messages, &Model::default(), &params, threshold)
    }

    fn tool_round(id: &str, words: usize) -> Message {
        use harnx_core::tool::{ToolCall, ToolResult};
        Message::new(
            MessageRole::Tool,
            MessageContent::ToolCalls(MessageContentToolCalls {
                text: "checking".into(),
                thought: None,
                sequence: false,
                rounds: Vec::new(),
                tool_results: vec![ToolResult {
                    call: ToolCall::new("read".into(), json!({}), Some(id.into()), None),
                    output: json!({"text": "word ".repeat(words)}),
                    markdown: None,
                    content: vec![],
                    switch_agent: None,
                    execution_context: None,
                }],
            }),
        )
    }

    #[test]
    fn keep_plan_membership_and_count_match_indices() {
        for len in 0..=6 {
            for tail_from in 0..=len + 1 {
                for prompt_idx in std::iter::once(None).chain((0..=len).map(Some)) {
                    let plan = KeepPlan {
                        prompt_idx,
                        tail_from,
                    };
                    let indices = plan.kept_indices(len);
                    assert_eq!(plan.kept_count(len), indices.len());
                    for index in 0..len {
                        assert_eq!(plan.contains(index), indices.contains(&index));
                    }
                }
            }
        }
    }

    #[test]
    fn compaction_handles_tool_first_sessions() {
        use MessageRole::*;
        let messages = vec![
            tool_round("old", 100),
            msg(User, "latest prompt"),
            tool_round("new", 100),
            msg(Assistant, "reply"),
        ];
        let budget = Model::default().messages_tokens(&[messages[1].clone(), messages[3].clone()]);
        assert_eq!(
            plan(&messages, 3, budget),
            KeepPlan {
                prompt_idx: Some(1),
                tail_from: 3
            }
        );
        let messages = vec![
            tool_round("old", 100),
            tool_round("new", 100),
            msg(Assistant, "reply"),
        ];
        let budget = Model::default().messages_tokens(&messages[2..]);
        assert_eq!(
            plan(&messages, 3, budget),
            KeepPlan {
                prompt_idx: None,
                tail_from: 2
            }
        );
        assert_eq!(
            plan(&messages, 3, 8000),
            KeepPlan {
                prompt_idx: None,
                tail_from: 1
            }
        );
    }

    #[test]
    fn compaction_keeps_recent_user_turns() {
        use MessageRole::*;
        let messages = vec![
            msg(System, "sys"),
            msg(User, "u1"),
            msg(Assistant, "a1"),
            msg(User, "u2"),
            msg(Assistant, "a2"),
            msg(User, "u3"),
            msg(Assistant, "a3"),
        ];
        assert_eq!(
            plan(&messages, 1, 100_000),
            KeepPlan {
                prompt_idx: None,
                tail_from: 5
            }
        );
        assert!(plan(&messages, 0, 100_000)
            .kept_indices(messages.len())
            .is_empty());
        assert_eq!(
            plan(&messages[1..], 100, 100_000),
            KeepPlan {
                prompt_idx: None,
                tail_from: 4
            }
        );
    }

    #[test]
    fn timing_recovery_compaction_counts_prompts_not_notices_as_turns() {
        use harnx_core::timing_identity::{TimingReceipt, TIMING_HEADER};
        use MessageRole::*;
        let receipt =
            TimingReceipt::initial("2026-01-15T12:00:00Z".parse().unwrap(), "active".into());
        let notice = msg(
            User,
            &format!("{TIMING_HEADER}{} | any prose", receipt.to_json()),
        );
        let messages = vec![
            msg(System, "sys"),
            msg(User, "u1"),
            notice.clone(),
            msg(Assistant, "a1"),
            msg(User, "u2"),
            notice.clone(),
            msg(Assistant, "a2"),
            msg(User, "u3"),
            notice,
            msg(Assistant, "a3"),
        ];
        for (turns, expected_tail) in [(1, 7), (2, 4), (100, 1)] {
            assert_eq!(plan(&messages, turns, 100_000).tail_from, expected_tail);
        }
        assert_eq!(plan(&messages[1..], 100, 100_000).tail_from, 6);
        for budget in 0..=Model::default().messages_tokens(&messages) {
            let p = plan(&messages, 2, budget);
            assert_timing_prompt_kept(&messages, p);
            assert_tail_boundary(&messages, p);
        }
    }

    fn assert_timing_prompt_kept(messages: &[Message], plan: KeepPlan) {
        let kept = plan.kept_indices(messages.len());
        assert!(!kept.is_empty(), "{plan:?}");
        assert!(kept.contains(&7), "latest prompt lost: {plan:?}");
        assert!(is_turn_prompt(&messages[kept[0]]));
    }

    #[test]
    fn compaction_keeps_prompt_and_latest_rounds_over_8k() {
        use MessageRole::*;
        let messages = vec![
            msg(User, "investigate"),
            tool_round("oldest", 2500),
            tool_round("old", 2500),
            tool_round("middle", 2500),
            tool_round("new", 2500),
            msg(Assistant, "found it"),
        ];
        assert!(Model::default().messages_tokens(&messages) > 8000);
        assert_eq!(
            plan(&messages, 3, 8000),
            KeepPlan {
                prompt_idx: Some(0),
                tail_from: 3
            }
        );
    }

    #[test]
    fn compaction_summarizes_oversized_round_but_keeps_reply() {
        use MessageRole::*;
        let messages = vec![
            msg(User, "investigate"),
            tool_round("huge", 10000),
            msg(Assistant, "found it"),
        ];
        assert_eq!(
            plan(&messages, 3, 8000).kept_indices(messages.len()),
            vec![0, 2]
        );
    }

    #[test]
    fn compaction_keeps_only_latest_prompt_separately() {
        use MessageRole::*;
        let messages = vec![
            msg(User, "old question"),
            msg(Assistant, "old answer"),
            msg(User, "latest question"),
            tool_round("huge", 10000),
            msg(Assistant, "found it"),
        ];
        assert_eq!(
            plan(&messages, 3, 8000),
            KeepPlan {
                prompt_idx: Some(2),
                tail_from: 4
            }
        );
    }

    #[test]
    fn compaction_prompt_over_budget_respects_half_threshold() {
        use MessageRole::*;
        let messages = vec![msg(User, &"word ".repeat(100)), msg(Assistant, "reply")];
        let model = Model::default();
        let tokens = model.messages_tokens(&messages[..1]);
        let cases = [
            (1, tokens * 2 + 1, Some(0)),
            (1, tokens * 2, None),
            (1, tokens, None),
            // A prompt within the keep budget doesn't need the threshold exception.
            (tokens, 0, Some(0)),
        ];
        for (budget, threshold, expected_prompt) in cases {
            assert_eq!(
                plan_with_threshold(&messages, 3, budget, threshold).prompt_idx,
                expected_prompt
            );
        }
    }

    #[test]
    fn compaction_single_prompt_session_compacts_at_least_one_message() {
        use MessageRole::*;
        let messages = vec![msg(User, "only prompt")];
        assert!(plan(&messages, 3, 8000).kept_indices(1).is_empty());
        let messages = vec![msg(User, "only prompt"), msg(Assistant, "reply")];
        assert_eq!(plan(&messages, 3, 8000).kept_indices(2), vec![0]);
        let messages = vec![
            msg(User, "only prompt"),
            tool_round("first", 10),
            tool_round("last", 10),
            msg(Assistant, "reply"),
        ];
        assert_eq!(plan(&messages, 3, 8000).kept_indices(4), vec![0, 2, 3]);
    }

    #[test]
    fn compaction_budget_sweep_keeps_atomic_rounds_and_compacts_something() {
        use MessageRole::*;
        let messages = vec![
            msg(User, "old prompt"),
            msg(Assistant, "old answer"),
            msg(User, "latest prompt"),
            tool_round("one", 20),
            tool_round("two", 30),
            msg(Assistant, "reply"),
        ];
        let model = Model::default();
        for budget in 0..=model.messages_tokens(&messages) {
            assert_budgeted_plan(&messages, plan(&messages, 3, budget), budget);
        }
        assert!(plan(&[], 3, 8000).kept_indices(0).is_empty());
    }

    fn assert_budgeted_plan(messages: &[Message], plan: KeepPlan, budget: usize) {
        let kept = plan.kept_indices(messages.len());
        let latest_prompt = messages.iter().rposition(is_turn_prompt).unwrap();
        assert!(kept.len() < messages.len(), "{plan:?}");
        assert!(kept.contains(&latest_prompt), "{plan:?}");
        assert!(plan
            .prompt_idx
            .is_none_or(|i| i == latest_prompt && i < plan.tail_from));
        assert_tail_boundary(messages, plan);
        assert_kept_rounds(messages, &kept);
        assert_kept_budget(messages, &kept, budget);
    }

    fn assert_tail_boundary(messages: &[Message], plan: KeepPlan) {
        assert!(plan.tail_from <= messages.len());
        if plan.prompt_idx.is_some() && plan.tail_from < messages.len() {
            assert!(is_round_boundary(&messages[plan.tail_from]));
        }
    }

    fn assert_kept_rounds(messages: &[Message], kept: &[usize]) {
        for &index in kept
            .iter()
            .filter(|&&i| messages[i].role == MessageRole::Tool)
        {
            assert_complete_round(&messages[index]);
            let position = kept.iter().position(|&i| i == index).unwrap();
            assert_eq!(
                &kept[position..],
                &(index..messages.len()).collect::<Vec<_>>()
            );
        }
    }

    fn assert_complete_round(message: &Message) {
        let MessageContent::ToolCalls(calls) = &message.content else {
            panic!("not a round")
        };
        assert_eq!(calls.tool_results.len(), 1);
        assert!(calls.tool_results[0].call.id.is_some());
        assert!(calls.tool_results[0].output.get("text").is_some());
    }

    fn assert_kept_budget(messages: &[Message], kept: &[usize], budget: usize) {
        let model = Model::default();
        let cost: usize = kept
            .iter()
            .map(|&i| model.messages_tokens(&messages[i..=i]))
            .sum();
        let latest_prompt = messages.iter().rposition(is_turn_prompt).unwrap();
        let prompt_cost = model.messages_tokens(&messages[latest_prompt..=latest_prompt]);
        assert!(cost <= budget || (kept == [latest_prompt] && budget < prompt_cost));
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
                    rounds: Vec::new(),
                    tool_results: vec![ToolResult {
                        call: ToolCall::new(
                            "bash".into(),
                            json!({"cmd": "ls"}),
                            Some("c1".into()),
                            None,
                        ),
                        output: json!({"_meta": {"harnx.dev/compaction": "3 files"}}),
                        markdown: None,
                        content: vec![],
                        switch_agent: None,
                        execution_context: None,
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

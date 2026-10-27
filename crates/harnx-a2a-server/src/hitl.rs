//! Plain-text HITL approval challenges and replies.
// These APIs are wired into the publisher and admission paths in Task 7/8.
#![allow(dead_code)]

use a2a_lf::{Part, PartContent};
use harnx_runtime::PendingHitlApproval;

const ARGUMENTS_LIMIT: usize = 8 * 1024;
const TRUNCATION_MARKER: &str = "\n… (arguments truncated)";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReplyParse {
    Approve(Option<String>),
    Reject(Option<String>),
    Unrecognized,
}

pub(crate) fn render_challenge(approval: &PendingHitlApproval) -> String {
    let arguments = serde_json::to_string_pretty(&approval.arguments)
        .unwrap_or_else(|_| "<could not render arguments>".to_owned());
    let arguments = truncate_utf8(&arguments, ARGUMENTS_LIMIT);
    format!(
        "Tool approval required\n\nTool: `{}`\n\nArguments:\n```json\n{}\n```\n\nSummary/reason: {}\n\nReply `y` (optionally followed by a note for the audit log) to approve, or `n` followed by a reason for the agent to reject. Approval notes are audit-only; rejection reasons are shown to the agent.",
        approval.tool_name, arguments, approval.summary
    )
}

pub(crate) fn render_unrecognized(approval: &PendingHitlApproval) -> String {
    format!(
        "Reply not recognized as approval or rejection.\n\n{}",
        render_challenge(approval)
    )
}

pub(crate) fn parse_reply(parts: &[Part]) -> ReplyParse {
    let mut text = String::new();
    for part in parts {
        match &part.content {
            PartContent::Text(value) => text.push_str(value),
            _ => return ReplyParse::Unrecognized,
        }
    }
    let trimmed = text.trim();
    let (token, remainder) = split_first_whitespace(trimmed);
    let token = token.trim_end_matches(['.', '!', ',']).to_ascii_lowercase();
    let note = nonempty(remainder);
    match token.as_str() {
        "y" | "yes" | "go" | "approve" => ReplyParse::Approve(note),
        "n" | "no" | "reject" | "deny" => ReplyParse::Reject(note),
        _ => ReplyParse::Unrecognized,
    }
}

fn split_first_whitespace(text: &str) -> (&str, &str) {
    match text.find(char::is_whitespace) {
        Some(end) => (&text[..end], text[end..].trim()),
        None => (text, ""),
    }
}

fn nonempty(text: &str) -> Option<String> {
    (!text.is_empty()).then(|| text.to_owned())
}

fn truncate_utf8(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit.saturating_sub(TRUNCATION_MARKER.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &text[..end], TRUNCATION_MARKER)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn approval(arguments: serde_json::Value) -> PendingHitlApproval {
        PendingHitlApproval {
            request_seq: 1,
            tool_round_seq: 2,
            tool_call_id: "call-1".into(),
            tool_name: "shell".into(),
            arguments,
            summary: "Run command".into(),
        }
    }

    #[test]
    fn parses_text_replies() {
        let cases = [
            ("Y", ReplyParse::Approve(None)),
            (
                "yes please do it",
                ReplyParse::Approve(Some("please do it".into())),
            ),
            ("no.", ReplyParse::Reject(None)),
            ("yesterday", ReplyParse::Unrecognized),
            ("", ReplyParse::Unrecognized),
        ];
        for (text, expected) in cases {
            assert_eq!(parse_reply(&[Part::text(text)]), expected, "{text:?}");
        }
    }

    #[test]
    fn rejects_non_text_parts() {
        let data = Part::data(json!({"approved": true}));
        assert_eq!(parse_reply(&[data]), ReplyParse::Unrecognized);
    }

    #[test]
    fn renderer_includes_tool_and_truncates_arguments() {
        let rendered = render_challenge(&approval(json!({"payload": "é".repeat(6000)})));
        assert!(rendered.contains("Tool: `shell`"));
        assert!(rendered.contains("arguments truncated"));
        assert!(rendered.len() < 8 * 1024 + 1024);
        assert!(render_unrecognized(&approval(json!({})))
            .starts_with("Reply not recognized as approval or rejection."));
    }
}

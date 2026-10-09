//! Tool-call and result transcript construction shared by live and restored sessions.

use crate::strip_ansi;
use crate::types::{ToolCallBody, TranscriptItem};
use harnx_core::event::{ToolEvent, ToolStatus};
use harnx_runtime::utils::pretty_yaml_block;

/// Build the body for a `TranscriptItem::ToolCall` from a `Started`
/// event's `markdown` and `input`. A non-empty rendered template `markdown`
/// becomes `ToolCallBody::Markdown`; otherwise the raw input is YAML-
/// formatted (or omitted entirely when input is `null`).
pub(super) fn tool_call_body(
    markdown: Option<&str>,
    input: &serde_json::Value,
) -> Option<crate::types::ToolCallBody> {
    match markdown.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => Some(crate::types::ToolCallBody::Markdown(t.to_string())),
        None => match input {
            serde_json::Value::Null => None,
            _ => Some(crate::types::ToolCallBody::Yaml(pretty_yaml_block(input))),
        },
    }
}

/// Convert a `Completed` event's `output` + `markdown` into transcript items.
/// The whole multi-line text is wrapped in a single `ToolResultMarkdown`
/// item so `markdown_lines` can parse block-level constructs — fenced
/// code (e.g. the ```diff blocks emitted by harnx-fs-tools / harnx-bash-tools
/// for history diffs), inline emphasis from a templated MCP
/// `result_template`, and plain text alike. Strips ANSI escapes from
/// string outputs before extraction so pre-dimmed test inputs render
/// cleanly.
pub(crate) fn full_tool_result_detail(output: &serde_json::Value) -> String {
    let raw = match output {
        serde_json::Value::String(s) => serde_json::Value::String(strip_ansi(s)),
        _ => output.clone(),
    };
    let text = harnx_core::tool::extract_all_display_text(&raw).unwrap_or_else(|| match &raw {
        serde_json::Value::String(s) => s.clone(),
        _ => harnx_runtime::utils::pretty_yaml_block(&raw),
    });
    strip_ansi(&text).trim_end_matches('\n').to_string()
}

pub(crate) fn full_detail_if_extra(full: String, text: &str) -> Option<String> {
    if full.trim().is_empty() || full.trim() == text.trim() {
        None
    } else {
        Some(full)
    }
}

pub(super) fn tool_completed_to_transcript_items(
    output: &serde_json::Value,
    markdown: Option<&str>,
) -> Vec<TranscriptItem> {
    let raw = match output {
        serde_json::Value::String(s) => serde_json::Value::String(strip_ansi(s)),
        _ => output.clone(),
    };
    let text = crate::agent_event_sink::render_tool_result_text(&raw, markdown);
    let clean = strip_ansi(&text).trim_end_matches('\n').to_string();
    if clean.is_empty() {
        return vec![];
    }
    let full = full_tool_result_detail(output);
    let mut items = vec![TranscriptItem::ToolResultMarkdown {
        full_detail: full_detail_if_extra(full.clone(), &clean),
        subagent_reply_owner: None,
        text: clean.clone(),
        rendered_cache: None,
    }];
    // Templates can add links that aren't present in the structured output.
    // Inspect the untruncated template, or all result content when there's no template.
    let link_source = markdown
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(strip_ansi)
        .unwrap_or(full);
    crate::lifecycle::append_markdown_links(&mut items, &link_source);
    items
}

pub(super) fn tool_started_to_transcript_items(
    event: ToolEvent,
    seq: Option<usize>,
) -> Vec<TranscriptItem> {
    let ToolEvent::Started {
        id,
        name,
        kind,
        markdown,
        input,
        locations,
    } = event
    else {
        return vec![];
    };
    tool_call_with_links(TranscriptItem::ToolCall {
        tool_name: name,
        body: tool_call_body(markdown.as_deref(), &input),
        seq,
        timestamp: Some(chrono::Utc::now()),
        id: Some(id),
        start_anchor: std::time::Instant::now(),
        final_elapsed_ms: None,
        rendered_cache: None,
        title: None,
        status: None,
        kind: Some(kind),
        locations,
        usage: None,
    })
}

pub(super) fn blocked_tool_to_transcript_items(
    event: ToolEvent,
    seq: Option<usize>,
) -> Vec<TranscriptItem> {
    let ToolEvent::Blocked {
        id,
        name,
        input,
        reason,
    } = event
    else {
        return vec![];
    };
    let input_text = match input {
        serde_json::Value::Null => String::new(),
        _ => format!("{}\n", pretty_yaml_block(&input)),
    };
    tool_call_with_links(TranscriptItem::ToolCall {
        tool_name: name,
        body: Some(ToolCallBody::Markdown(format!(
            "{input_text}⊘ blocked: {reason}"
        ))),
        seq,
        timestamp: Some(chrono::Utc::now()),
        id: Some(id),
        start_anchor: std::time::Instant::now(),
        // Blocked calls are terminal at creation; never show a running timer.
        final_elapsed_ms: Some(0),
        rendered_cache: None,
        title: None,
        status: Some(ToolStatus::Failed),
        kind: None,
        locations: vec![],
        usage: None,
    })
}

fn tool_call_with_links(item: TranscriptItem) -> Vec<TranscriptItem> {
    let mut links = Vec::new();
    if let TranscriptItem::ToolCall {
        body: Some(ToolCallBody::Markdown(markdown)),
        ..
    } = &item
    {
        crate::lifecycle::append_markdown_links(&mut links, markdown);
    }
    std::iter::once(item).chain(links).collect()
}

#[cfg(test)]
mod tests {
    use super::tool_completed_to_transcript_items;
    use crate::types::TranscriptItem;
    use serde_json::json;

    #[test]
    fn tool_completed_preserves_fenced_diff_in_transcript() {
        let output = json!({
            "content": [
                {
                    "type": "text",
                    "text": "Applied patch successfully"
                },
                {
                    "type": "text",
                    "text": "```diff\n-old line\n+new line\n```"
                }
            ],
            "isError": false
        });

        let items = tool_completed_to_transcript_items(&output, None);

        assert_eq!(items.len(), 1);
        match &items[0] {
            TranscriptItem::ToolResultMarkdown { text, .. } => {
                assert!(text.contains("Applied patch successfully"));
                assert!(text.contains("```diff"));
                assert!(text.contains("-old line"));
                assert!(text.contains("+new line"));
            }
            other => panic!("unexpected transcript item: {other:?}"),
        }
    }

    #[test]
    fn tool_completed_extracts_markdown_link() {
        let items = tool_completed_to_transcript_items(
            &serde_json::json!("Output [Tool Link](https://tool.com)"),
            None,
        );
        assert!(items.iter().any(|item| matches!(
            item,
            TranscriptItem::MarkdownLink { text, url }
                if text == "Tool Link" && url == "https://tool.com"
        )));
    }

    #[test]
    fn tool_completed_extracts_link_from_full_detail_beyond_preview() {
        // Create a long output where link appears beyond typical preview truncation
        let mut padding = String::new();
        for _ in 0..5000 {
            padding.push('x');
        }
        let output = format!(
            "{}\n\n[Hidden Link](https://hidden.example.com)\n{}",
            padding, padding
        );
        let items = tool_completed_to_transcript_items(&serde_json::json!(output), None);
        // Link should be extracted from full detail even if preview truncates
        assert!(items.iter().any(|item| matches!(
            item,
            TranscriptItem::MarkdownLink { text, url }
                if text == "Hidden Link" && url == "https://hidden.example.com"
        )));
    }
}

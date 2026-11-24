//! The `harnx_agent_session_history_read` built-in tool: lets an agent search
//! its own NATS session log (including pre-compaction entries) so detail
//! dropped by compaction stays recoverable. Pure query core here; the
//! `ToolProvider` wiring reads the active session's NATS stream.

use anyhow::Result;
use fancy_regex::Regex;
use harnx_core::session::SessionLogEntry;
use serde_json::{json, Value};

mod provider;
pub use provider::{tool_declaration, SessionHistoryProvider};

/// The tool name exposed to agents.
pub const TOOL_NAME: &str = "harnx_agent_session_history_read";

/// Structured filters for a history query. All fields optional; absent = no
/// constraint. `jaq` is applied (if present) to the array of matched rows.
#[derive(Debug, Default, Clone)]
pub struct HistoryQuery {
    pub index_min: Option<usize>,
    pub index_max: Option<usize>,
    pub entry_type: Option<String>,
    pub tool_name: Option<String>,
    pub text_regex: Option<String>,
    pub limit: Option<usize>,
    pub jaq: Option<String>,
}

/// The log-entry `type` discriminant string (matches the YAML `type:` tag).
pub fn entry_type(entry: &SessionLogEntry) -> &'static str {
    match entry {
        SessionLogEntry::Message { .. } => "message",
        SessionLogEntry::ToolCalls { .. } => "tool_calls",
        SessionLogEntry::ToolResults { .. } => "tool_results",
        SessionLogEntry::DataUrls { .. } => "data_urls",
        SessionLogEntry::Compress { .. } => "compress",
        SessionLogEntry::Clear => "clear",
        SessionLogEntry::Cancel { .. } => "cancel",
        SessionLogEntry::Error { .. } => "error",
        SessionLogEntry::TurnEnd { .. } => "turn_end",
        SessionLogEntry::SubAgentStarted { .. } => "sub_agent_started",
        SessionLogEntry::HandoffCommitted { .. } => "handoff_committed",
        SessionLogEntry::HitlApprovalRequested { .. } => "hitl_approval_requested",
        SessionLogEntry::HitlApprovalDecision { .. } => "hitl_approval_decision",
        SessionLogEntry::ToolCallCompleted { .. } => "tool_call_completed",
        SessionLogEntry::InteractionRequested { .. } => "interaction_requested",
        SessionLogEntry::SuspensionReady { .. } => "suspension_ready",
        SessionLogEntry::InteractionResponse { .. } => "interaction_response",
        SessionLogEntry::SurfaceAction { .. } => "surface_action",
        SessionLogEntry::EditEntries { .. } => "edit_entries",
        SessionLogEntry::Rewind { .. } => "rewind",
        SessionLogEntry::CompactRequest { .. } => "compact_request",
        SessionLogEntry::CompactResult { .. } => "compact_result",
        SessionLogEntry::AdmissionClosed { .. } => "admission_closed",
        SessionLogEntry::Unknown => "unknown",
    }
}

/// Best-effort plain-text rendering of an entry, for `text_regex` matching and
/// the row's `text` field. Bounded; image `cid:` refs are included as attachment markers.
pub(crate) fn entry_searchable_text(entry: &SessionLogEntry) -> String {
    match entry {
        SessionLogEntry::Message { content, .. } => content.to_transcript_text(),
        SessionLogEntry::ToolCalls { text, calls, .. } => tool_calls_text(text, calls),
        SessionLogEntry::ToolResults { results, .. } => tool_results_text(results),
        SessionLogEntry::Compress { prompt } => prompt.clone(),
        SessionLogEntry::Cancel {
            fence_token,
            requested_by,
            ..
        } => match requested_by {
            Some(by) => format!("cancel (fence {fence_token}, by {by})"),
            None => format!("cancel (fence {fence_token})"),
        },
        SessionLogEntry::Error { message, .. } => message.clone(),
        SessionLogEntry::SubAgentStarted {
            agent,
            session_id,
            invocation_id,
            tool_call_id,
            started_at,
        } => sub_agent_started_text(
            agent,
            session_id,
            invocation_id.as_deref(),
            tool_call_id.as_deref(),
            *started_at,
        ),
        SessionLogEntry::HandoffCommitted {
            target_agent,
            target_session_id,
            handoff_tool_call_id,
        } => handoff_committed_text(
            target_agent,
            target_session_id,
            handoff_tool_call_id.as_deref(),
        ),
        SessionLogEntry::HitlApprovalRequested {
            tool_call_id,
            summary,
            ..
        } => format!("{tool_call_id}\n{summary}"),
        SessionLogEntry::HitlApprovalDecision {
            tool_call_id,
            approved,
            note,
            ..
        } => format!(
            "{tool_call_id}\n{approved}\n{}",
            note.as_deref().unwrap_or_default()
        ),
        SessionLogEntry::SurfaceAction {
            surface_id,
            action,
            note_text,
            ..
        } => format!(
            "{surface_id}\n{action}\n{}",
            note_text.as_deref().unwrap_or_default()
        ),
        _ => String::new(),
    }
}

fn tool_calls_text(text: &str, calls: &[harnx_core::tool::ToolCall]) -> String {
    let calls_text: Vec<String> = calls
        .iter()
        .map(|c| format!("{}({})", c.name, c.arguments))
        .collect();
    format!("{text}\n{}", calls_text.join("\n"))
}

fn tool_results_text(results: &[harnx_core::session::ToolOutput]) -> String {
    results
        .iter()
        .map(|r| format!("{}: {}", r.name, r.output))
        .collect::<Vec<_>>()
        .join("\n")
}

fn sub_agent_started_text(
    agent: &str,
    session_id: &str,
    invocation_id: Option<&str>,
    tool_call_id: Option<&str>,
    started_at: Option<chrono::DateTime<chrono::Utc>>,
) -> String {
    format!(
        "{agent}\n{session_id}\n{}\n{}\n{}",
        invocation_id.unwrap_or_default(),
        tool_call_id.unwrap_or_default(),
        started_at
            .map(|value| value.to_rfc3339())
            .unwrap_or_default()
    )
}

fn handoff_committed_text(
    target_agent: &str,
    target_session_id: &str,
    handoff_tool_call_id: Option<&str>,
) -> String {
    match handoff_tool_call_id {
        Some(tool_call_id) => format!("{target_agent}\n{target_session_id}\n{tool_call_id}"),
        None => format!("{target_agent}\n{target_session_id}"),
    }
}

/// Tool names referenced by an entry (for `tool_name` filtering).
pub(crate) fn entry_tool_names(entry: &SessionLogEntry) -> Vec<String> {
    match entry {
        SessionLogEntry::ToolCalls { calls, .. } => calls.iter().map(|c| c.name.clone()).collect(),
        SessionLogEntry::ToolResults { results, .. } => {
            results.iter().map(|r| r.name.clone()).collect()
        }
        _ => Vec::new(),
    }
}

/// Render one entry to a JSON row: `{ seq, type, text?, tool_names?, role? }`.
fn entry_row(seq: usize, entry: &SessionLogEntry) -> Value {
    let mut row = json!({ "seq": seq, "type": entry_type(entry) });
    let text = entry_searchable_text(entry);
    if !text.is_empty() {
        row["text"] = json!(text);
    }
    let tools = entry_tool_names(entry);
    if !tools.is_empty() {
        row["tool_names"] = json!(tools);
    }
    if let SessionLogEntry::Message { role, .. } = entry {
        row["role"] = json!(format!("{role:?}").to_lowercase());
    }
    row
}

/// Apply structured filters (index range, type, tool name, text regex, limit)
/// to `entries`, returning a JSON array of matched rows. `jaq` is applied in a
/// later layer, not here.
pub fn query_entries(entries: &[(usize, SessionLogEntry)], query: &HistoryQuery) -> Result<Value> {
    let regex = query
        .text_regex
        .as_deref()
        .map(Regex::new)
        .transpose()
        .map_err(|e| anyhow::anyhow!("invalid text_regex: {e}"))?;

    let mut rows: Vec<Value> = Vec::new();
    for (seq, entry) in entries {
        if query.index_min.is_some_and(|m| *seq < m) {
            continue;
        }
        if query.index_max.is_some_and(|m| *seq > m) {
            continue;
        }
        if let Some(t) = &query.entry_type {
            if entry_type(entry) != t {
                continue;
            }
        }
        if let Some(tool) = &query.tool_name {
            if !entry_tool_names(entry).iter().any(|n| n == tool) {
                continue;
            }
        }
        if let Some(re) = &regex {
            if !re.is_match(&entry_searchable_text(entry)).unwrap_or(false) {
                continue;
            }
        }
        rows.push(entry_row(*seq, entry));
    }
    if let Some(limit) = query.limit {
        rows.truncate(limit);
    }
    Ok(Value::Array(rows))
}

/// Like `query_entries`, then apply the optional `jaq` expression over the
/// matched-rows array. A broken jaq expression is surfaced as an error (rather
/// than silently returning unfiltered rows) so the calling agent can fix or
/// drop it instead of acting on results it wrongly believes were filtered.
pub fn query_entries_with_jaq(
    entries: &[(usize, SessionLogEntry)],
    query: &HistoryQuery,
) -> Result<Value> {
    let rows = query_entries(entries, query)?;
    let Some(expr) = &query.jaq else {
        return Ok(rows);
    };
    // Report the jaq message itself so the agent can fix the expression instead
    // of guessing. A filter that simply matches nothing is not an error, so this
    // won't misfire on legitimately-empty results.
    harnx_core::jaq::eval_filter_checked(expr, rows).map_err(|message| anyhow::anyhow!(message))
}

/// Parse a session log document string and run the full query against it.
pub fn query_log_content(content: &str, session_name: &str, query: &HistoryQuery) -> Result<Value> {
    let entries = crate::config::session::collect_raw_log_entries(content, session_name)?;
    query_entries_with_jaq(&entries, query)
}

#[cfg(test)]
mod tests {
    use super::provider::parse_query;
    use super::*;
    use harnx_core::message::{ImageUrl, MessageContent, MessageContentPart, MessageRole};

    #[test]
    fn entry_type_maps_variants() {
        assert_eq!(
            entry_type(&SessionLogEntry::Message {
                id: None,
                role: MessageRole::User,
                content: MessageContent::Text("hi".into()),
                timestamp: None,
                fence_token: None,
            }),
            "message"
        );
        assert_eq!(
            entry_type(&SessionLogEntry::Compress { prompt: "s".into() }),
            "compress"
        );
    }

    #[test]
    fn message_attachment_markers_are_searchable() {
        let entries = vec![
            (
                1,
                SessionLogEntry::Message {
                    id: Some("image-only".to_string()),
                    role: MessageRole::User,
                    content: MessageContent::Array(vec![MessageContentPart::ImageUrl {
                        image_url: ImageUrl {
                            url: "cid:image-only".to_string(),
                        },
                    }]),
                    timestamp: None,
                    fence_token: None,
                },
            ),
            (
                2,
                SessionLogEntry::Message {
                    id: Some("text-and-image".to_string()),
                    role: MessageRole::Assistant,
                    content: MessageContent::Array(vec![
                        MessageContentPart::Text {
                            text: "caption".to_string(),
                        },
                        MessageContentPart::ImageUrl {
                            image_url: ImageUrl {
                                url: "cid:text-and-image".to_string(),
                            },
                        },
                    ]),
                    timestamp: None,
                    fence_token: None,
                },
            ),
        ];

        assert_eq!(
            entry_searchable_text(&entries[0].1),
            "[image attachment: cid:image-only]"
        );
        assert_eq!(
            entry_searchable_text(&entries[1].1),
            "caption\n\n[image attachment: cid:text-and-image]"
        );

        let rows = query_entries(
            &entries,
            &HistoryQuery {
                text_regex: Some("cid:text-and-image".to_string()),
                ..HistoryQuery::default()
            },
        )
        .expect("attachment marker search succeeds");
        assert_eq!(
            rows,
            json!([{
                "seq": 2,
                "type": "message",
                "text": "caption\n\n[image attachment: cid:text-and-image]",
                "role": "assistant",
            }])
        );
    }

    #[test]
    fn sub_agent_started_is_typed_and_searchable() {
        let entry = SessionLogEntry::SubAgentStarted {
            agent: "pantheon/plato".into(),
            session_id: "child-123".into(),
            invocation_id: Some("invocation-456".into()),
            tool_call_id: Some("call-789".into()),
            started_at: None,
        };

        assert_eq!(entry_type(&entry), "sub_agent_started");
        let text = entry_searchable_text(&entry);
        assert!(text.contains("pantheon/plato"));
        assert!(text.contains("child-123"));
        assert!(text.contains("invocation-456"));
    }

    #[test]
    fn durable_control_entries_are_typed_and_searchable() {
        let entries = [
            SessionLogEntry::HandoffCommitted {
                target_agent: "pantheon/plato".into(),
                target_session_id: "target-123".into(),
                handoff_tool_call_id: Some("handoff-call".into()),
            },
            SessionLogEntry::HitlApprovalRequested {
                tool_call_id: "call-request".into(),
                summary: "Approve write".into(),
                fence_token: 1,
            },
            SessionLogEntry::HitlApprovalDecision {
                tool_call_id: "call-decision".into(),
                approved: false,
                note: Some("Narrow the path".into()),
                fence_token: 1,
            },
        ];

        let expected = [
            ("handoff_committed", "handoff-call"),
            ("hitl_approval_requested", "Approve write"),
            ("hitl_approval_decision", "Narrow the path"),
        ];
        for (entry, (expected_type, expected_text)) in entries.iter().zip(expected) {
            assert_eq!(entry_type(entry), expected_type);
            assert!(entry_searchable_text(entry).contains(expected_text));
        }
    }

    fn sample_entries() -> Vec<(usize, SessionLogEntry)> {
        use harnx_core::tool::ToolCall;
        use serde_json::json;
        vec![
            (
                0,
                SessionLogEntry::Message {
                    id: None,
                    role: MessageRole::User,
                    content: MessageContent::Text("please run the build".into()),
                    timestamp: None,
                    fence_token: None,
                },
            ),
            (
                1,
                SessionLogEntry::ToolCalls {
                    text: "running it".into(),
                    thought: None,
                    calls: vec![ToolCall::new(
                        "bash".into(),
                        json!({"cmd":"make"}),
                        Some("c1".into()),
                        None,
                    )],
                    timestamp: None,
                    fence_token: None,
                },
            ),
            (
                2,
                SessionLogEntry::ToolResults {
                    handoff_selection: None,
                    results: vec![harnx_core::session::ToolOutput {
                        user_surfaces: Vec::new(),
                        id: Some("c1".into()),
                        name: "bash".into(),
                        output: json!({"content":[{"type":"text","text":"build ok"}]}),
                        markdown: None,
                        content: vec![],
                        switch_agent: None,
                    }],
                    timestamp: None,
                },
            ),
            (
                3,
                SessionLogEntry::Message {
                    id: None,
                    role: MessageRole::Assistant,
                    content: MessageContent::Text("the build passed".into()),
                    timestamp: None,
                    fence_token: None,
                },
            ),
        ]
    }

    #[test]
    fn query_filters_by_type() {
        let q = HistoryQuery {
            entry_type: Some("message".into()),
            ..Default::default()
        };
        let rows = query_entries(&sample_entries(), &q).unwrap();
        let arr = rows.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert!(arr.iter().all(|r| r["type"] == "message"));
    }

    #[test]
    fn query_filters_by_tool_name_and_index_range() {
        let q = HistoryQuery {
            tool_name: Some("bash".into()),
            ..Default::default()
        };
        let rows = query_entries(&sample_entries(), &q).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 2);

        let q = HistoryQuery {
            index_min: Some(2),
            index_max: Some(3),
            ..Default::default()
        };
        let rows = query_entries(&sample_entries(), &q).unwrap();
        let seqs: Vec<u64> = rows
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["seq"].as_u64().unwrap())
            .collect();
        assert_eq!(seqs, vec![2, 3]);
    }

    #[test]
    fn query_filters_by_text_regex_and_limit() {
        let q = HistoryQuery {
            text_regex: Some("build".into()),
            ..Default::default()
        };
        let rows = query_entries(&sample_entries(), &q).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 3);

        let q = HistoryQuery {
            text_regex: Some("build".into()),
            limit: Some(1),
            ..Default::default()
        };
        let rows = query_entries(&sample_entries(), &q).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 1);
    }

    #[test]
    fn query_applies_jaq_expression() {
        let q = HistoryQuery {
            jaq: Some("map(select(.type == \"tool_results\"))".into()),
            ..Default::default()
        };
        let rows = query_entries_with_jaq(&sample_entries(), &q).unwrap();
        let arr = rows.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["type"], "tool_results");
    }

    #[test]
    fn query_invalid_jaq_is_surfaced_as_error() {
        let q = HistoryQuery {
            jaq: Some("this is (not valid jaq".into()),
            ..Default::default()
        };
        let err = query_entries_with_jaq(&sample_entries(), &q).unwrap_err();
        assert!(err.to_string().contains("jaq"));
    }

    #[test]
    fn tool_declaration_has_expected_name_and_params() {
        let decl = tool_declaration();
        assert_eq!(decl.name, TOOL_NAME);
        let props = decl.parameters.properties().expect("schema has properties");
        assert!(props.contains_key("jaq"));
        assert!(props.contains_key("type"));
        assert!(props.contains_key("tool_name"));
    }

    #[test]
    fn parse_query_maps_arguments() {
        let q = parse_query(&json!({"type": "message", "limit": 5, "index_min": 2}));
        assert_eq!(q.entry_type.as_deref(), Some("message"));
        assert_eq!(q.limit, Some(5));
        assert_eq!(q.index_min, Some(2));
        assert_eq!(q.index_max, None);
        assert_eq!(q.tool_name, None);
        assert_eq!(q.text_regex, None);
        assert_eq!(q.jaq, None);
    }

    #[test]
    fn query_from_log_content_parses_and_filters() {
        let log = "type: header\nmodel: openai:gpt-4o\n---\ntype: message\nrole: user\ncontent: hello world\n";
        let q = HistoryQuery {
            entry_type: Some("message".into()),
            ..Default::default()
        };
        let rows = query_log_content(log, "test", &q).unwrap();
        let arr = rows.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert!(arr[0]["text"].as_str().unwrap().contains("hello world"));
    }

    #[test]
    fn query_surfaces_entries_before_a_compress_boundary() {
        let log = concat!(
            "type: header\nmodel: openai:gpt-4o\n",
            "---\ntype: message\nrole: user\ncontent: original question\n",
            "---\ntype: compress\nprompt: summary so far\n",
            "---\ntype: message\nrole: assistant\ncontent: recent answer\n",
        );
        let rows = query_log_content(log, "test", &HistoryQuery::default()).unwrap();
        let arr = rows.as_array().unwrap();
        // The pre-compaction "original question" entry is still surfaced.
        assert!(arr.iter().any(|r| r["text"]
            .as_str()
            .is_some_and(|t| t.contains("original question"))));
        assert!(arr.iter().any(|r| r["type"] == "compress"));
        assert!(arr.iter().any(|r| r["text"]
            .as_str()
            .is_some_and(|t| t.contains("recent answer"))));
    }
}

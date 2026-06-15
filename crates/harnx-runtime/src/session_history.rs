//! The `harnx_agent_session_history_read` built-in tool: lets an agent search
//! its own session's on-disk log (including pre-compaction entries) so detail
//! dropped by compaction stays recoverable. Pure query core here; the
//! `ToolProvider` wiring resolves the active session path and reads the file.

use anyhow::Result;
use fancy_regex::Regex;
use harnx_core::session::SessionLogEntry;
use serde_json::{json, Value};

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
        SessionLogEntry::Header { .. } => "header",
        SessionLogEntry::Message { .. } => "message",
        SessionLogEntry::ToolCalls { .. } => "tool_calls",
        SessionLogEntry::ToolResults { .. } => "tool_results",
        SessionLogEntry::DataUrls { .. } => "data_urls",
        SessionLogEntry::Compress { .. } => "compress",
        SessionLogEntry::Clear => "clear",
        SessionLogEntry::EditEntries { .. } => "edit_entries",
        SessionLogEntry::Rewind { .. } => "rewind",
        SessionLogEntry::Unknown => "unknown",
    }
}

/// Best-effort plain-text rendering of an entry, for `text_regex` matching and
/// the row's `text` field. Bounded; images are already `cid:` refs.
fn entry_searchable_text(entry: &SessionLogEntry) -> String {
    match entry {
        SessionLogEntry::Message { content, .. } => content.to_text(),
        SessionLogEntry::ToolCalls { text, calls, .. } => {
            let calls_text: Vec<String> =
                calls.iter().map(|c| format!("{}({})", c.name, c.arguments)).collect();
            format!("{text}\n{}", calls_text.join("\n"))
        }
        SessionLogEntry::ToolResults { results, .. } => results
            .iter()
            .map(|r| format!("{}: {}", r.name, r.output))
            .collect::<Vec<_>>()
            .join("\n"),
        SessionLogEntry::Compress { prompt } => prompt.clone(),
        SessionLogEntry::Header { model_id, .. } => format!("model: {model_id}"),
        _ => String::new(),
    }
}

/// Tool names referenced by an entry (for `tool_name` filtering).
fn entry_tool_names(entry: &SessionLogEntry) -> Vec<String> {
    match entry {
        SessionLogEntry::ToolCalls { calls, .. } => calls.iter().map(|c| c.name.clone()).collect(),
        SessionLogEntry::ToolResults { results, .. } => results.iter().map(|r| r.name.clone()).collect(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::message::{MessageContent, MessageRole};

    #[test]
    fn entry_type_maps_variants() {
        assert_eq!(
            entry_type(&SessionLogEntry::Message {
                role: MessageRole::User,
                content: MessageContent::Text("hi".into()),
                timestamp: None,
            }),
            "message"
        );
        assert_eq!(
            entry_type(&SessionLogEntry::Compress { prompt: "s".into() }),
            "compress"
        );
    }

    fn sample_entries() -> Vec<(usize, SessionLogEntry)> {
        use harnx_core::tool::ToolCall;
        use serde_json::json;
        vec![
            (0, SessionLogEntry::Message {
                role: MessageRole::User,
                content: MessageContent::Text("please run the build".into()),
                timestamp: None,
            }),
            (1, SessionLogEntry::ToolCalls {
                text: "running it".into(),
                thought: None,
                calls: vec![ToolCall { name: "bash".into(), arguments: json!({"cmd":"make"}), id: Some("c1".into()), thought_signature: None }],
                timestamp: None,
            }),
            (2, SessionLogEntry::ToolResults {
                results: vec![harnx_core::session::ToolOutput {
                    id: Some("c1".into()), name: "bash".into(),
                    output: json!({"content":[{"type":"text","text":"build ok"}]}),
                    content: vec![], switch_agent: None,
                }],
                timestamp: None,
            }),
            (3, SessionLogEntry::Message {
                role: MessageRole::Assistant,
                content: MessageContent::Text("the build passed".into()),
                timestamp: None,
            }),
        ]
    }

    #[test]
    fn query_filters_by_type() {
        let q = HistoryQuery { entry_type: Some("message".into()), ..Default::default() };
        let rows = query_entries(&sample_entries(), &q).unwrap();
        let arr = rows.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert!(arr.iter().all(|r| r["type"] == "message"));
    }

    #[test]
    fn query_filters_by_tool_name_and_index_range() {
        let q = HistoryQuery { tool_name: Some("bash".into()), ..Default::default() };
        let rows = query_entries(&sample_entries(), &q).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 2);

        let q = HistoryQuery { index_min: Some(2), index_max: Some(3), ..Default::default() };
        let rows = query_entries(&sample_entries(), &q).unwrap();
        let seqs: Vec<u64> = rows.as_array().unwrap().iter().map(|r| r["seq"].as_u64().unwrap()).collect();
        assert_eq!(seqs, vec![2, 3]);
    }

    #[test]
    fn query_filters_by_text_regex_and_limit() {
        let q = HistoryQuery { text_regex: Some("build".into()), ..Default::default() };
        let rows = query_entries(&sample_entries(), &q).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 3);

        let q = HistoryQuery { text_regex: Some("build".into()), limit: Some(1), ..Default::default() };
        let rows = query_entries(&sample_entries(), &q).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 1);
    }
}

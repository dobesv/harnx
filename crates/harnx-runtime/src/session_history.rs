//! The `harnx_agent_session_history_read` built-in tool: lets an agent search
//! its own session's on-disk log (including pre-compaction entries) so detail
//! dropped by compaction stays recoverable. Pure query core here; the
//! `ToolProvider` wiring resolves the active session path and reads the file.

use crate::config::GlobalConfig;
use anyhow::Result;
use async_trait::async_trait;
use fancy_regex::Regex;
use harnx_core::abort::AbortSignal;
use harnx_core::session::SessionLogEntry;
use harnx_core::tool::{JsonSchema, ToolDeclaration, ToolError, ToolProvider};
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
        SessionLogEntry::Cancel { .. } => "cancel",
        SessionLogEntry::Error { .. } => "error",
        SessionLogEntry::EditEntries { .. } => "edit_entries",
        SessionLogEntry::Rewind { .. } => "rewind",
        SessionLogEntry::Title { .. } => "title",
        SessionLogEntry::Unknown => "unknown",
    }
}

/// Best-effort plain-text rendering of an entry, for `text_regex` matching and
/// the row's `text` field. Bounded; images are already `cid:` refs.
fn entry_searchable_text(entry: &SessionLogEntry) -> String {
    match entry {
        SessionLogEntry::Message { content, .. } => content.to_text(),
        SessionLogEntry::ToolCalls { text, calls, .. } => {
            let calls_text: Vec<String> = calls
                .iter()
                .map(|c| format!("{}({})", c.name, c.arguments))
                .collect();
            format!("{text}\n{}", calls_text.join("\n"))
        }
        SessionLogEntry::ToolResults { results, .. } => results
            .iter()
            .map(|r| format!("{}: {}", r.name, r.output))
            .collect::<Vec<_>>()
            .join("\n"),
        SessionLogEntry::Compress { prompt } => prompt.clone(),
        SessionLogEntry::Title { title, .. } => title.clone(),
        SessionLogEntry::Header { model_id, .. } => format!("model: {model_id}"),
        SessionLogEntry::Error { message, .. } => message.clone(),
        _ => String::new(),
    }
}

/// Tool names referenced by an entry (for `tool_name` filtering).
fn entry_tool_names(entry: &SessionLogEntry) -> Vec<String> {
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

/// Build the tool declaration (name, description, JSON-schema parameters).
pub fn tool_declaration() -> ToolDeclaration {
    let schema = json!({
        "type": "object",
        "properties": {
            "index_min": {"type": "integer", "description": "Minimum log entry seq (inclusive)."},
            "index_max": {"type": "integer", "description": "Maximum log entry seq (inclusive)."},
            "type": {"type": "string", "description": "Filter by entry type: message, tool_calls, tool_results, compress, header, data_urls, clear, edit_entries, rewind."},
            "tool_name": {"type": "string", "description": "Keep only entries referencing this tool name."},
            "text_regex": {"type": "string", "description": "Keep entries whose rendered text matches this regular expression."},
            "limit": {"type": "integer", "description": "Maximum number of rows to return."},
            "jaq": {"type": "string", "description": "Optional jq/jaq expression applied to the array of matched rows for arbitrary filtering or projection. Invalid expressions return an error."}
        }
    });
    ToolDeclaration {
        name: TOOL_NAME.to_string(),
        description: "Search this session's own history log, including detail dropped by compaction. \
Filter by entry index range, type, tool name, or a text regex; optionally refine with a jaq expression. \
Returns a JSON array of matching log entries (seq, type, text, tool names)."
            .to_string(),
        parameters: serde_json::from_value::<JsonSchema>(schema)
            .expect("session-history tool schema is valid"),
        mcp_tool_name: None,
        mcp_server_name: None,
        call_template: None,
        result_template: None,
        idempotent_hint: None,
        read_only_hint: None,
    }
}

/// Parse the tool-call arguments JSON into a `HistoryQuery`.
fn parse_query(arguments: &Value) -> HistoryQuery {
    let s = |k: &str| {
        arguments
            .get(k)
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let u = |k: &str| {
        arguments
            .get(k)
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
    };
    HistoryQuery {
        index_min: u("index_min"),
        index_max: u("index_max"),
        entry_type: s("type"),
        tool_name: s("tool_name"),
        text_regex: s("text_regex"),
        limit: u("limit"),
        jaq: s("jaq"),
    }
}

/// `ToolProvider` for the session-history tool. Resolves the active session's
/// on-disk log path from the captured config at call time (own session only).
pub struct SessionHistoryProvider {
    config: GlobalConfig,
}

impl SessionHistoryProvider {
    pub fn new(config: GlobalConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl ToolProvider for SessionHistoryProvider {
    fn name(&self) -> &str {
        "session_history"
    }

    fn has_tool(&self, tool_name: &str) -> bool {
        tool_name == TOOL_NAME
    }

    async fn call_tool(
        &self,
        _tool_name: &str,
        arguments: Value,
        _abort: &AbortSignal,
    ) -> Result<Value, ToolError> {
        let (path, name) = {
            let guard = self.config.read();
            let session = guard
                .session
                .as_ref()
                .ok_or_else(|| ToolError::Recoverable(anyhow::anyhow!("no active session")))?;
            let path = session.path.clone().ok_or_else(|| {
                ToolError::Recoverable(anyhow::anyhow!("session has not been saved yet"))
            })?;
            (path, session.id().to_string())
        };
        let content = std::fs::read_to_string(&path).map_err(|e| {
            ToolError::Recoverable(anyhow::anyhow!("failed to read session log: {e}"))
        })?;
        let query = parse_query(&arguments);
        let rows = query_log_content(&content, &name, &query).map_err(ToolError::Recoverable)?;
        Ok(json!({ "content": [{ "type": "text", "text": rows.to_string() }] }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::message::{MessageContent, MessageRole};

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
                    calls: vec![ToolCall {
                        name: "bash".into(),
                        arguments: json!({"cmd":"make"}),
                        id: Some("c1".into()),
                        thought_signature: None,
                    }],
                    timestamp: None,
                    fence_token: None,
                },
            ),
            (
                2,
                SessionLogEntry::ToolResults {
                    results: vec![harnx_core::session::ToolOutput {
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
        let props = decl
            .parameters
            .properties
            .as_ref()
            .expect("schema has properties");
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

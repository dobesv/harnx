//! The `harnx_agent_session_history_read` built-in tool: lets an agent search
//! its own session's on-disk log (including pre-compaction entries) so detail
//! dropped by compaction stays recoverable. Pure query core here; the
//! `ToolProvider` wiring resolves the active session path and reads the file.

use harnx_core::session::SessionLogEntry;

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
}

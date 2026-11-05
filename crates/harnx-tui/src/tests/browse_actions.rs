//! Tests for transcript browse-mode actions (`e`/`d`/`r`/`c`/`i`) and their
//! footer notices, grouped by responsibility:
//! - `seq_assignment`: live log-seq binding for user text and tool rounds
//! - `action_notices`: key outcomes — refusal notices, confirmations, ranges
//! - `footer_notices`: notice lifecycle, clearing, and rendering

use super::*;

mod action_notices;
mod footer_notices;
mod seq_assignment;

/// Declarative transcript row for the browse tests. Keeps the shared helpers
/// free of string arguments and the call sites compact.
#[derive(Clone)]
enum RowSpec {
    /// Static user row with an optional seq.
    User {
        text: &'static str,
        seq: Option<usize>,
    },
    /// Live user row (timestamp set, seq None) created during a session.
    LiveUser { text: &'static str },
    /// Live assistant row (timestamp set, seq None) created during a session.
    LiveAssistant { text: &'static str },
    /// Static assistant row with an optional seq.
    Assistant {
        text: &'static str,
        seq: Option<usize>,
    },
    /// Static completed `bash_exec` tool-call row with an optional seq.
    ToolCall { seq: Option<usize> },
    /// Static completed sub-agent row (no copyable text).
    SubAgent,
}

fn row(spec: &RowSpec) -> TranscriptItem {
    let now = std::time::Instant::now();
    match spec {
        RowSpec::User { text, seq } => TranscriptItem::UserText {
            text: text.to_string(),
            seq: *seq,
            timestamp: None,
        },
        RowSpec::LiveUser { text } => TranscriptItem::UserText {
            text: text.to_string(),
            seq: None,
            timestamp: Some(chrono::Utc::now()),
        },
        RowSpec::LiveAssistant { text } => TranscriptItem::AssistantText {
            text: text.to_string(),
            seq: None,
            timestamp: Some(chrono::Utc::now()),
            rendered_cache: None,
        },
        RowSpec::Assistant { text, seq } => TranscriptItem::AssistantText {
            text: text.to_string(),
            seq: *seq,
            timestamp: None,
            rendered_cache: None,
        },
        RowSpec::ToolCall { seq } => TranscriptItem::ToolCall {
            tool_name: "bash_exec".to_string(),
            body: None,
            seq: *seq,
            timestamp: None,
            id: None,
            start_anchor: now,
            final_elapsed_ms: Some(0),
            rendered_cache: None,
            title: None,
            status: Some(ToolStatus::Completed),
            kind: None,
            locations: vec![],
            usage: None,
        },
        RowSpec::SubAgent => TranscriptItem::SubAgentSession {
            key: crate::types::MonitoredSessionKey {
                cluster: "local".to_string(),
                agent: "metis".to_string(),
                session_id: "child-1".to_string(),
            },
            status: crate::types::SubAgentStatus::Completed,
            invocation_id: None,
            progress: None,
        },
    }
}

/// Harness in browsing mode over `specs`, focused on `focus` with an optional
/// selection anchor.
async fn browsing_harness(
    specs: Vec<RowSpec>,
    focus: usize,
    anchor: Option<usize>,
) -> TuiTestHarness {
    let mut harness = TuiTestHarness::new().await;
    harness.tui().app.transcript.clear();
    for spec in specs {
        harness.tui().app.transcript.push(row(&spec));
    }
    harness.tui().app.transcript_focus = Some(focus);
    harness.tui().app.transcript_selection_anchor = anchor;
    harness.tui().app.transcript_browsing = true;
    harness
}

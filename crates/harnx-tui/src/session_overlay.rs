//! Session overlay rendering helpers for TUI.
//!
//! Provides formatting for `.info session` (metadata) and `.dump session` (transcript)
//! overlay display in the TUI.

use anyhow::Result;
use harnx_core::event::{AgentEvent, ContentBlock, ModelEvent, NoticeEvent, ToolEvent, UserEvent};
use harnx_core::session::SessionLogEntry;
use harnx_runtime::nats_session::replay_default_view_to_sink;
use harnx_runtime::utils::pretty_yaml_block;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::strip_ansi;

/// Render the default transcript view from the complete raw session log.
pub fn render_transcript_text(entries: &[(u64, SessionLogEntry)]) -> Result<String> {
    let sink = Arc::new(BufferSink::default());
    replay_default_view_to_sink(entries, 0, &HashMap::new(), sink.clone())?;
    Ok(sink.lines())
}

/// Internal sink that collects transcript events into formatted text lines.
#[derive(Default)]
struct BufferSink(Mutex<Vec<String>>);

impl BufferSink {
    fn lines(&self) -> String {
        self.0.lock().unwrap().join("\n\n")
    }
}

impl harnx_core::event::AgentEventSink for BufferSink {
    fn emit(&self, event: AgentEvent) {
        let mut buf = self.0.lock().unwrap();
        match event {
            AgentEvent::User(UserEvent::Message { content }) => {
                buf.push(format!("── user ──\n{content}"));
            }
            AgentEvent::Model(ModelEvent::Final { output, .. }) => {
                buf.push(format!("── assistant ──\n{output}"));
            }
            AgentEvent::Model(ModelEvent::MessageChunk { blocks }) => {
                let text = concat_text_blocks(&blocks);
                if !text.is_empty() {
                    buf.push(format!("── chunk ──\n{text}"));
                }
            }
            AgentEvent::Tool(ToolEvent::Started {
                name,
                input,
                markdown,
                ..
            }) => {
                let input_str = markdown.or_else(|| {
                    if input.is_null() {
                        None
                    } else {
                        Some(pretty_yaml_block(&input))
                    }
                });
                match input_str {
                    Some(s) if !s.is_empty() => buf.push(format!("── tool call ──\n→ {name}\n{s}")),
                    _ => buf.push(format!("── tool call ──\n→ {name}")),
                }
            }
            AgentEvent::Tool(ToolEvent::Completed {
                output, markdown, ..
            }) => {
                let text =
                    crate::agent_event_sink::render_tool_result_text(&output, markdown.as_deref());
                let clean = strip_ansi(&text).trim_end_matches('\n').to_string();
                if !clean.is_empty() {
                    buf.push(format!("── tool result ──\n{clean}"));
                }
            }
            AgentEvent::Notice(notice) => buf.push(notice_text(notice)),
            _ => {}
        }
    }
}

fn notice_text(notice: NoticeEvent) -> String {
    match notice {
        NoticeEvent::Info(message) => message.trim_end_matches('\n').to_owned(),
        NoticeEvent::Warning(message) => format!("⚠ {message}"),
        NoticeEvent::Error(message) => format!("error: {message}"),
    }
}

/// Concatenate text content blocks into a single string.
fn concat_text_blocks(blocks: &[ContentBlock]) -> String {
    let mut out = String::new();
    for block in blocks {
        if let ContentBlock::Text(t) = block {
            out.push_str(t);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::api_types::CompletionTokenUsage;
    use harnx_core::event::{AgentEventSink, ToolKind};
    use harnx_core::message::MessageRole;

    fn text_entry(role: MessageRole, content: &str) -> (u64, SessionLogEntry) {
        (
            1,
            SessionLogEntry::Message {
                id: None,
                role,
                content: harnx_core::message::MessageContent::Text(content.into()),
                timestamp: None,
                fence_token: Some(0),
            },
        )
    }

    #[test]
    fn render_transcript_text_formats_user_message() {
        let entries = vec![text_entry(MessageRole::User, "Hello")];
        let output = render_transcript_text(&entries).unwrap();
        assert!(output.contains("── user ──"));
        assert!(output.contains("Hello"));
    }

    #[test]
    fn render_transcript_text_formats_assistant_message() {
        let entries = vec![text_entry(MessageRole::Assistant, "Hi there")];
        let output = render_transcript_text(&entries).unwrap();
        assert!(output.contains("── assistant ──"));
        assert!(output.contains("Hi there"));
    }

    #[test]
    fn render_transcript_text_formats_notice_warning() {
        let entries = vec![(
            1,
            SessionLogEntry::Error {
                message: "test warning".into(),
                fence_token: 0,
                timestamp: None,
            },
        )];
        let output = render_transcript_text(&entries).unwrap();
        // Error entries are replayed as notice events
        assert!(!output.is_empty());
    }

    #[test]
    fn buffer_sink_formats_user_message_event() {
        let sink = Arc::new(BufferSink::default());
        sink.emit(AgentEvent::User(UserEvent::Message {
            content: "User asks a question".into(),
        }));
        let output = sink.lines();
        assert!(output.contains("── user ──"), "output: {output}");
        assert!(output.contains("User asks a question"), "output: {output}");
    }

    #[test]
    fn buffer_sink_formats_assistant_final_message_event() {
        let sink = Arc::new(BufferSink::default());
        sink.emit(AgentEvent::Model(ModelEvent::Final {
            output: "Assistant response text".into(),
            usage: CompletionTokenUsage::new(Some(10), Some(20), None),
        }));
        let output = sink.lines();
        assert!(output.contains("── assistant ──"), "output: {output}");
        assert!(
            output.contains("Assistant response text"),
            "output: {output}"
        );
    }

    #[test]
    fn buffer_sink_formats_message_chunk_event() {
        let sink = Arc::new(BufferSink::default());
        sink.emit(AgentEvent::Model(ModelEvent::MessageChunk {
            blocks: vec![ContentBlock::Text("streaming text".into())],
        }));
        let output = sink.lines();
        assert!(output.contains("── chunk ──"), "output: {output}");
        assert!(output.contains("streaming text"), "output: {output}");
    }

    #[test]
    fn buffer_sink_formats_tool_started_event() {
        let sink = Arc::new(BufferSink::default());
        sink.emit(AgentEvent::Tool(ToolEvent::Started {
            id: "call-1".into(),
            name: "bash_exec".into(),
            kind: ToolKind::Other,
            markdown: None,
            input: serde_json::json!({"command": "echo hello"}),
            locations: vec![],
        }));
        let output = sink.lines();
        assert!(output.contains("── tool call ──"), "output: {output}");
        assert!(output.contains("→ bash_exec"), "output: {output}");
    }

    #[test]
    fn buffer_sink_formats_tool_completed_event() {
        let sink = Arc::new(BufferSink::default());
        sink.emit(AgentEvent::Tool(ToolEvent::Completed {
            id: "call-1".into(),
            output: serde_json::json!({"result": "hello"}),
            markdown: Some("```\nhello\n```".into()),
        }));
        let output = sink.lines();
        assert!(output.contains("── tool result ──"), "output: {output}");
    }

    #[test]
    fn buffer_sink_formats_info_notice_event() {
        let sink = Arc::new(BufferSink::default());
        sink.emit(AgentEvent::Notice(NoticeEvent::Info(
            "earlier history archived\n".into(),
        )));
        assert_eq!(sink.lines(), "earlier history archived");
    }

    #[test]
    fn buffer_sink_formats_notice_error_event() {
        let sink = Arc::new(BufferSink::default());
        sink.emit(AgentEvent::Notice(NoticeEvent::Error(
            "something went wrong".into(),
        )));
        let output = sink.lines();
        assert!(output.contains("error:"), "output: {output}");
        assert!(output.contains("something went wrong"), "output: {output}");
    }

    #[test]
    fn buffer_sink_formats_notice_warning_event() {
        let sink = Arc::new(BufferSink::default());
        sink.emit(AgentEvent::Notice(NoticeEvent::Warning(
            "deprecation notice".into(),
        )));
        let output = sink.lines();
        assert!(output.contains("⚠"), "output: {output}");
        assert!(output.contains("deprecation notice"), "output: {output}");
    }
}

#[cfg(test)]
mod compaction_tests;

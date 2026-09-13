//! Session overlay rendering helpers for TUI.
//!
//! Provides formatting for `.info session` (metadata) and `.dump session` (transcript)
//! overlay display in the TUI.

use harnx_core::event::{AgentEvent, ContentBlock, ModelEvent, NoticeEvent, ToolEvent, UserEvent};
use harnx_core::session::SessionLogEntry;
use harnx_runtime::nats_session::replay_entries_to_sink;
use harnx_runtime::utils::pretty_yaml_block;
use std::sync::{Arc, Mutex};

use crate::strip_ansi;

/// Render session transcript entries as text for overlay display.
pub fn render_transcript_text(entries: &[(u64, SessionLogEntry)]) -> String {
    let sink = Arc::new(BufferSink::default());
    replay_entries_to_sink(entries, sink.clone());
    sink.lines()
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
            AgentEvent::Notice(NoticeEvent::Warning(msg)) => {
                buf.push(format!("⚠ {msg}"));
            }
            AgentEvent::Notice(NoticeEvent::Error(msg)) => {
                buf.push(format!("error: {msg}"));
            }
            _ => {}
        }
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
        let output = render_transcript_text(&entries);
        assert!(output.contains("── user ──"));
        assert!(output.contains("Hello"));
    }

    #[test]
    fn render_transcript_text_formats_assistant_message() {
        let entries = vec![text_entry(MessageRole::Assistant, "Hi there")];
        let output = render_transcript_text(&entries);
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
        let output = render_transcript_text(&entries);
        // Error entries are replayed as notice events
        assert!(!output.is_empty());
    }
}

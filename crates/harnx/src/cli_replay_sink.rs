//! Replay presentation adapter for text dumps and follow output.
//!
//! Replay Info notices contain untrusted summaries; live Info notices can
//! contain renderer-generated ANSI and keep their existing sink path.

use crate::cli_event_sink::CliAgentEventSink;
use harnx_core::event::{AgentEvent, AgentEventSink, NoticeEvent};

/// Terminal presentation boundary for text dumps and follow output.
pub struct CliReplayEventSink(CliAgentEventSink);

impl Default for CliReplayEventSink {
    fn default() -> Self {
        Self(CliAgentEventSink::new(
            false,
            harnx_render::RenderOptions::default(),
            harnx_core::abort::create_abort_signal(),
        ))
    }
}

impl AgentEventSink for CliReplayEventSink {
    fn emit(&self, mut event: AgentEvent) {
        if let AgentEvent::Notice(NoticeEvent::Info(message)) = &mut event {
            sanitize_replay_notice(message);
        }
        self.0.emit(event);
    }
}

/// Strip terminal control characters from replayed notices, preserving LF and TAB.
fn sanitize_replay_notice(message: &mut String) {
    message.retain(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_control_chars_except_newline_and_tab() {
        let mut msg = "line1\x1b[2J\t\r\x07line2\n".to_string();
        sanitize_replay_notice(&mut msg);
        assert_eq!(msg, "line1[2J\tline2\n");
    }
}

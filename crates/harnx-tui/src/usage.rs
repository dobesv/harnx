use crate::render_helpers::render_usage_line;
use crate::types::{TranscriptItem, Tui};
use harnx_core::event::{AgentSource, ModelEvent};

impl Tui {
    pub(super) fn render_usage_event(
        &mut self,
        source: Option<&AgentSource>,
        event: ModelEvent,
    ) -> Vec<TranscriptItem> {
        let ModelEvent::Usage {
            input,
            output,
            cached,
            session_label,
            ..
        } = event
        else {
            unreachable!("render_usage_event requires a usage event")
        };
        let Some(line) = render_usage_line(input, output, cached, session_label.as_deref(), source)
        else {
            return vec![];
        };
        if self.update_existing_usage_line(source, &line) {
            vec![]
        } else {
            vec![TranscriptItem::UsageLine(line)]
        }
    }
}

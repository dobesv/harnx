use harnx_core::event::{AgentEvent, AgentEventSink, ContentBlock, ModelEvent};
use harnx_runtime::NatsTurnResult;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

pub(crate) struct AssistantTextTrackingSink {
    inner: Arc<dyn AgentEventSink>,
    rendered_assistant_text: AtomicBool,
}

impl AssistantTextTrackingSink {
    pub(crate) fn new(inner: Arc<dyn AgentEventSink>) -> Self {
        Self {
            inner,
            rendered_assistant_text: AtomicBool::new(false),
        }
    }

    pub(crate) fn emit_durable_response_if_needed(&self, result: NatsTurnResult) {
        if result.was_cancelled || self.rendered_assistant_text.load(Ordering::Acquire) {
            return;
        }
        if let Some(response) = result.response.filter(|response| !response.is_empty()) {
            self.emit(AgentEvent::Model(ModelEvent::MessageChunk {
                blocks: vec![ContentBlock::Text(response)],
            }));
        }
    }
}

impl AgentEventSink for AssistantTextTrackingSink {
    fn emit(&self, event: AgentEvent) {
        if event_has_assistant_text(&event) {
            self.rendered_assistant_text.store(true, Ordering::Release);
        }
        self.inner.emit(event);
    }
}

fn event_has_assistant_text(event: &AgentEvent) -> bool {
    match event {
        AgentEvent::Model(ModelEvent::MessageChunk { blocks }) => blocks
            .iter()
            .any(|block| matches!(block, ContentBlock::Text(text) if !text.is_empty())),
        AgentEvent::SubAgent { event, .. } => event_has_assistant_text(event),
        _ => false,
    }
}

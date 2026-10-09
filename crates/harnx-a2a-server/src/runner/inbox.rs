//! Synchronous runtime sink coalesces text without an unbounded event queue.
use super::event_map::A2aEventSink;
use anyhow::{ensure, Result};
use harnx_core::event::{AgentEvent, ContentBlock, ModelEvent};
use tokio::sync::Notify;

#[derive(Default)]
struct Buffered {
    text: String,
    overflow: bool,
}
pub(super) struct TextInbox {
    buffered: parking_lot::Mutex<Buffered>,
    ready: Notify,
    limit: usize,
}
impl TextInbox {
    pub fn new(limit: usize) -> Self {
        Self {
            buffered: Default::default(),
            ready: Notify::new(),
            limit: if limit == 0 { 1024 * 1024 } else { limit },
        }
    }
    pub async fn ready(&self) {
        self.ready.notified().await;
    }
    pub fn take(&self) -> Result<String> {
        let mut buffered = self.buffered.lock();
        ensure!(
            !buffered.overflow,
            "coalesced task output exceeds NATS payload budget"
        );
        Ok(std::mem::take(&mut buffered.text))
    }
    fn append(&self, text: &str) {
        let mut buffered = self.buffered.lock();
        if text.len() > self.limit.saturating_sub(buffered.text.len()) {
            buffered.overflow = true;
        } else if !buffered.overflow {
            buffered.text.push_str(text);
        }
        drop(buffered);
        self.ready.notify_one();
    }
}
impl harnx_core::event::AgentEventSink for A2aEventSink {
    fn emit(&self, event: AgentEvent) {
        if let AgentEvent::Model(ModelEvent::MessageChunk { blocks }) = event {
            for block in blocks {
                if let ContentBlock::Text(text) = block {
                    self.0.append(&text);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::event::AgentEventSink;

    #[tokio::test]
    async fn text_coalesces_and_notifies_without_unbounded_messages() {
        harnx_core::require_nextest();
        let inbox = std::sync::Arc::new(TextInbox::new(8));
        let sink = A2aEventSink(inbox.clone());
        sink.emit(AgentEvent::Model(ModelEvent::MessageChunk {
            blocks: vec![ContentBlock::Text("abc".into())],
        }));
        sink.emit(AgentEvent::Model(ModelEvent::MessageChunk {
            blocks: vec![ContentBlock::Text("def".into())],
        }));
        inbox.ready().await;
        assert_eq!(inbox.take().unwrap(), "abcdef");
        assert_eq!(inbox.take().unwrap(), "");
    }
    #[test]
    fn full_inbox_and_total_output_fail_explicitly_without_growth() {
        harnx_core::require_nextest();
        let inbox = TextInbox::new(8);
        inbox.append("abcdefgh");
        inbox.append("overflow");
        assert!(inbox.take().is_err());
        assert_eq!(inbox.buffered.lock().text.len(), 8);
        let mut output = super::super::event_map::Output::default();
        output.accept("abcdefgh".into(), 8).unwrap();
        assert!(output.accept("overflow".into(), 8).is_err());
        assert_eq!(output.text.len(), 8);
    }
}

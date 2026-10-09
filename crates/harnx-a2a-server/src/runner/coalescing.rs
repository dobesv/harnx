//! Collect runtime text until the next artifact deadline, without publishing per chunk.
//!
//! Artifact coalescing groups rapid model output into fewer SSE events. The publisher
//! uses ArtifactCoalescer with ARTIFACT_INTERVAL (100ms) to accumulate TextInbox chunks
//! and emit one combined delta per tick. Verified by artifact_coalescing_waits_100ms_and_emits_pending_chunks_together.
use super::{event_map::Output, inbox::TextInbox, ARTIFACT_INTERVAL};
use anyhow::Result;
use std::sync::Arc;
use tokio::time::{Interval, MissedTickBehavior};

pub(super) struct ArtifactCoalescer {
    inbox: Arc<TextInbox>,
    interval: Interval,
    limit: usize,
}

impl ArtifactCoalescer {
    pub(super) fn new(inbox: Arc<TextInbox>, limit: usize) -> Self {
        let mut interval = tokio::time::interval(ARTIFACT_INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        Self {
            inbox,
            interval,
            limit,
        }
    }

    // Cancellation of this future leaves accepted text in Output. The publisher's
    // outer select can observe cancellation or completion without losing a delta.
    pub(super) async fn ready(&mut self, output: &mut Output) -> Result<()> {
        loop {
            tokio::select! {
                _ = self.inbox.ready() => output.accept(self.inbox.take()?, self.limit)?,
                _ = self.interval.tick() => return Ok(()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2a_lf::{Task, TaskArtifactUpdateEvent};
    use futures::FutureExt;
    use harnx_core::event::{AgentEvent, AgentEventSink, ContentBlock, ModelEvent};
    use std::time::Duration;
    use tokio::time::{advance, Instant};

    fn emit(sink: &impl AgentEventSink, text: &str) {
        sink.emit(AgentEvent::Model(ModelEvent::MessageChunk {
            blocks: vec![ContentBlock::Text(text.into())],
        }));
    }

    fn assert_delta(update: &TaskArtifactUpdateEvent, text: &str, append: bool) {
        let wire = serde_json::to_value(update).unwrap();
        assert_eq!(wire["taskId"], "task");
        assert_eq!(wire["contextId"], "context");
        assert_eq!(wire["artifact"]["artifactId"], "answer");
        assert_eq!(
            wire["artifact"]["parts"],
            serde_json::json!([{"text": text}])
        );
        assert_eq!(update.append, Some(append));
        assert_eq!(update.last_chunk, Some(false));
    }

    #[tokio::test(start_paused = true)]
    async fn artifact_coalescing_waits_100ms_and_emits_pending_chunks_together() -> Result<()> {
        harnx_core::require_nextest();
        let task: Task = serde_json::from_value(serde_json::json!({
            "id": "task", "contextId": "context", "status": {"state": "TASK_STATE_WORKING"}
        }))?;
        let inbox = Arc::new(TextInbox::new(1024));
        let sink = super::super::event_map::A2aEventSink(inbox.clone());
        let mut coalescer = ArtifactCoalescer::new(inbox, 1024);
        let mut output = Output::default();
        let start = Instant::now();

        // Tokio's first tick is immediate. Consume it before feeding text.
        assert!(coalescer.ready(&mut output).now_or_never().unwrap().is_ok());
        assert!(output.artifact_update(&task, false, false).is_none());

        advance(Duration::from_millis(1)).await;
        emit(&sink, "a");
        assert!(coalescer.ready(&mut output).now_or_never().is_none());
        advance(Duration::from_millis(39)).await;
        emit(&sink, "b");
        assert!(coalescer.ready(&mut output).now_or_never().is_none());
        assert_eq!(
            output.pending, "ab",
            "two separately accepted chunks must remain pending"
        );
        advance(Duration::from_millis(59)).await;
        assert_eq!(Instant::now() - start, Duration::from_millis(99));
        assert!(coalescer.ready(&mut output).now_or_never().is_none());
        advance(Duration::from_millis(1)).await;
        assert!(coalescer.ready(&mut output).now_or_never().unwrap().is_ok());
        assert_delta(
            &output.artifact_update(&task, false, false).unwrap(),
            "ab",
            false,
        );
        output.committed();
        assert!(output.pending.is_empty());
        assert_eq!(output.text, "ab");

        advance(Duration::from_millis(1)).await;
        emit(&sink, "c");
        assert!(coalescer.ready(&mut output).now_or_never().is_none());
        advance(Duration::from_millis(39)).await;
        emit(&sink, "d");
        assert!(coalescer.ready(&mut output).now_or_never().is_none());
        assert_eq!(output.pending, "cd");
        advance(Duration::from_millis(59)).await;
        assert_eq!(Instant::now() - start, Duration::from_millis(199));
        assert!(coalescer.ready(&mut output).now_or_never().is_none());
        advance(Duration::from_millis(1)).await;
        assert!(coalescer.ready(&mut output).now_or_never().unwrap().is_ok());
        assert_delta(
            &output.artifact_update(&task, false, false).unwrap(),
            "cd",
            true,
        );
        output.committed();
        assert_eq!(output.text, "abcd");

        // A later chunk has its own delta, not the previously committed text.
        advance(Duration::from_millis(41)).await;
        emit(&sink, "e");
        assert!(coalescer.ready(&mut output).now_or_never().is_none());
        advance(Duration::from_millis(59)).await;
        assert!(coalescer.ready(&mut output).now_or_never().unwrap().is_ok());
        assert_delta(
            &output.artifact_update(&task, false, false).unwrap(),
            "e",
            true,
        );
        output.committed();
        assert_eq!(output.text, "abcde");
        assert!(output.artifact_update(&task, false, false).is_none());
        Ok(())
    }
}

//! Apply the sequence fence and lifecycle checks when a queued event reaches the wire.
use ag_ui_core::event::Event;
use bytes::Bytes;
use harnx_core::event::{AgentEvent, NoticeEvent};
use harnx_runtime::nats_event_sink::{AdvisoryEnvelope, LiveEventState};
use tokio::sync::mpsc;
use tokio_stream::Stream;

use crate::ag_ui_lifecycle::{frame_guarded_live_event, LiveStreamGuard};

pub(crate) struct QueuedEvent {
    /// The `after_seq` of the advisory this event was mapped from: where the
    /// worker's output sat in the session log. It is re-checked here because an
    /// interrupt can land while the event waits for a full output channel.
    pub after_seq: u64,
    pub event: Event,
}

struct FrameQueue {
    rx: mpsc::Receiver<QueuedEvent>,
    live: LiveEventState,
    last_durable_seq: u64,
    guard: LiveStreamGuard,
}

/// `should_render` reads only the envelope's sequence, and a queued event has
/// already been mapped out of the agent event that carried it, so the fence is
/// re-applied through a probe envelope carrying just that sequence.
fn fence_probe(after_seq: u64) -> AdvisoryEnvelope {
    AdvisoryEnvelope::new(
        after_seq,
        AgentEvent::Notice(NoticeEvent::Info(String::new())),
    )
}

/// Finalize only lifecycles actually sent. Guarding before enqueue would emit
/// orphan ENDs if a stop discards a queued START under backpressure.
pub(crate) fn event_frames(
    rx: mpsc::Receiver<QueuedEvent>,
    live: LiveEventState,
    last_durable_seq: u64,
) -> impl Stream<Item = Bytes> + Send + Sync {
    let queue = FrameQueue {
        rx,
        live,
        last_durable_seq,
        guard: LiveStreamGuard::default(),
    };
    futures_util::stream::unfold(queue, |mut queue| async move {
        while let Some(queued) = queue.rx.recv().await {
            let fence = fence_probe(queued.after_seq);
            if queue.live.should_render(&fence, queue.last_durable_seq) {
                if let Some(frame) = frame_guarded_live_event(queued.event, &mut queue.guard) {
                    return Some((frame, queue));
                }
            }
        }
        let closes = queue.guard.finalize_open_lifecycles();
        (!closes.is_empty()).then_some((closes, queue))
    })
}

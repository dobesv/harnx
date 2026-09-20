//! Apply the sequence fence and lifecycle checks when a queued event reaches the wire.
use ag_ui_core::event::Event;
use bytes::Bytes;
use harnx_core::event::{AgentEvent, NoticeEvent};
use harnx_runtime::nats_event_sink::{AdvisoryEnvelope, LiveEventState};
use tokio::sync::mpsc;
use tokio_stream::Stream;

#[cfg(test)]
use crate::ag_ui_lifecycle::LiveStreamGuard;
use crate::{ag_ui::SharedLiveStreamGuard, ag_ui_events::frame_guarded_live_event};

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
    guard: SharedLiveStreamGuard,
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
#[cfg(test)]
pub(crate) fn event_frames(
    rx: mpsc::Receiver<QueuedEvent>,
    live: LiveEventState,
    last_durable_seq: u64,
) -> impl Stream<Item = Bytes> + Send + Sync {
    event_frames_with_guard(
        rx,
        live,
        last_durable_seq,
        std::sync::Arc::new(std::sync::Mutex::new(LiveStreamGuard::default())),
    )
}

pub(crate) fn event_frames_with_guard(
    rx: mpsc::Receiver<QueuedEvent>,
    live: LiveEventState,
    last_durable_seq: u64,
    guard: SharedLiveStreamGuard,
) -> impl Stream<Item = Bytes> + Send + Sync {
    let queue = FrameQueue {
        rx,
        live,
        last_durable_seq,
        guard,
    };
    futures_util::stream::unfold(queue, |mut queue| async move {
        while let Some(queued) = queue.rx.recv().await {
            let fence = fence_probe(queued.after_seq);
            if queue.live.should_render(&fence, queue.last_durable_seq) {
                let frame = frame_guarded_live_event(
                    queued.event,
                    &mut queue.guard.lock().expect("live stream guard"),
                );
                if let Some(frame) = frame {
                    return Some((frame, queue));
                }
            }
        }
        let closes = queue
            .guard
            .lock()
            .expect("live stream guard")
            .finalize_open_lifecycles();
        (!closes.is_empty()).then_some((closes, queue))
    })
}

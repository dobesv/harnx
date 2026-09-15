//! Apply generation and lifecycle checks when a queued event reaches the wire.
use ag_ui_core::event::Event;
use bytes::Bytes;
use harnx_runtime::nats_event_sink::LiveEventState;
use tokio::sync::mpsc;
use tokio_stream::Stream;

use crate::ag_ui_lifecycle::{frame_guarded_live_event, LiveStreamGuard};

pub(crate) struct QueuedEvent {
    pub generation: String,
    pub event: Event,
}

struct FrameQueue {
    rx: mpsc::Receiver<QueuedEvent>,
    live: LiveEventState,
    guard: LiveStreamGuard,
}

/// Finalize only lifecycles actually sent. Guarding before enqueue would emit
/// orphan ENDs if a stop discards a queued START under backpressure.
pub(crate) fn event_frames(
    rx: mpsc::Receiver<QueuedEvent>,
    live: LiveEventState,
) -> impl Stream<Item = Bytes> + Send + Sync {
    let queue = FrameQueue {
        rx,
        live,
        guard: LiveStreamGuard::default(),
    };
    futures_util::stream::unfold(queue, |mut queue| async move {
        while let Some(queued) = queue.rx.recv().await {
            if queue.live.allows(Some(&queued.generation)) {
                if let Some(frame) = frame_guarded_live_event(queued.event, &mut queue.guard) {
                    return Some((frame, queue));
                }
            }
        }
        let closes = queue.guard.finalize_open_lifecycles();
        (!closes.is_empty()).then_some((closes, queue))
    })
}

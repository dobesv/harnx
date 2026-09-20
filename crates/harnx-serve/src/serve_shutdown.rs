use std::{sync::Arc, time::Duration};

use bytes::Bytes;
use tokio_util::sync::CancellationToken;

use crate::ag_ui::{
    frame_terminal_after_lifecycle_closes, AgUiEventStream, GuardedEventStream,
    SharedLiveStreamGuard,
};
use crate::ag_ui_sync::frame_run_error_event;

/// Per-stream delay bounds used to stagger AG-UI reconnects during shutdown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamDrainConfig {
    min_jitter: Duration,
    max_jitter: Duration,
}

impl StreamDrainConfig {
    pub fn new(min_jitter: Duration, max_jitter: Duration) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !min_jitter.is_zero(),
            "stream shutdown minimum jitter must be greater than zero"
        );
        anyhow::ensure!(
            min_jitter <= max_jitter,
            "stream shutdown minimum jitter must not exceed maximum jitter"
        );
        Ok(Self {
            min_jitter,
            max_jitter,
        })
    }

    fn sample(&self) -> Duration {
        let min_nanos = self.min_jitter.as_nanos().min(u64::MAX as u128) as u64;
        let max_nanos = self.max_jitter.as_nanos().min(u64::MAX as u128) as u64;
        Duration::from_nanos(rand::random_range(min_nanos..=max_nanos))
    }
}

impl Default for StreamDrainConfig {
    fn default() -> Self {
        Self {
            min_jitter: Duration::from_secs(1),
            max_jitter: Duration::from_secs(20),
        }
    }
}

#[derive(Clone)]
pub(crate) struct ServeShutdown {
    token: CancellationToken,
    deadline: Arc<std::sync::OnceLock<std::time::Instant>>,
    stream_drain: StreamDrainConfig,
}

impl ServeShutdown {
    pub(crate) fn new(stream_drain: StreamDrainConfig) -> Self {
        Self {
            token: CancellationToken::new(),
            deadline: Arc::new(std::sync::OnceLock::new()),
            stream_drain,
        }
    }

    pub(crate) fn begin(&self, drain_timeout: Duration) {
        let _ = self.deadline.set(std::time::Instant::now() + drain_timeout);
        self.token.cancel();
    }

    pub(crate) async fn cancelled(&self) {
        self.token.cancelled().await;
    }

    /// Sample only after cancellation has been observed, then clamp the delay
    /// to the process drain deadline shared with Hyper's connection barrier.
    pub(crate) fn sample_stream_delay(&self) -> Duration {
        let sampled = self.stream_drain.sample();
        let remaining = self
            .deadline
            .get()
            .map(|deadline| deadline.saturating_duration_since(std::time::Instant::now()))
            .unwrap_or(Duration::ZERO);
        sampled.min(remaining)
    }
}

impl Default for ServeShutdown {
    fn default() -> Self {
        Self::new(StreamDrainConfig::default())
    }
}

fn is_run_terminal_frame(frame: &Bytes) -> bool {
    #[derive(serde::Deserialize)]
    struct EventType {
        #[serde(rename = "type")]
        event_type: String,
    }

    frame
        .as_ref()
        .split(|byte| *byte == b'\n')
        .filter_map(|line| line.strip_prefix(b"data:"))
        .filter_map(|data| serde_json::from_slice::<EventType>(data).ok())
        .any(|event| matches!(event.event_type.as_str(), "RUN_FINISHED" | "RUN_ERROR"))
}

pub(crate) fn close_stream_on_shutdown(
    guarded: GuardedEventStream,
    shutdown: ServeShutdown,
    thread_id: &str,
    run_id: &str,
) -> AgUiEventStream {
    struct State {
        stream: AgUiEventStream,
        guard: SharedLiveStreamGuard,
        shutdown: ServeShutdown,
        thread_id: String,
        run_id: String,
        done: bool,
    }

    let state = State {
        stream: guarded.stream,
        guard: guarded.guard,
        shutdown,
        thread_id: thread_id.to_string(),
        run_id: run_id.to_string(),
        done: false,
    };
    Box::pin(futures_util::stream::unfold(
        state,
        |mut state| async move {
            if state.done {
                return None;
            }
            tokio::select! {
                frame = tokio_stream::StreamExt::next(&mut state.stream) => {
                    frame.map(|frame| {
                        state.done = is_run_terminal_frame(&frame);
                        (frame, state)
                    })
                }
                _ = state.shutdown.cancelled() => {
                    let delay = state.shutdown.sample_stream_delay();
                    tokio::time::sleep(delay).await;
                    let terminal = Bytes::from(frame_run_error_event(
                        &state.thread_id,
                        &state.run_id,
                        "Server is shutting down; reconnect to continue the run.",
                    ));
                    let frame = frame_terminal_after_lifecycle_closes(
                        &mut state.guard.lock().expect("live stream guard"),
                        terminal,
                    );
                    state.done = true;
                    Some((frame, state))
                }
            }
        },
    ))
}
#[cfg(test)]
mod tests {
    use crate::ag_ui_events::frame_guarded_live_event;
    use crate::ag_ui_lifecycle::LiveStreamGuard;
    use ag_ui_core::{
        event::{BaseEvent, Event, TextMessageStartEvent},
        types::{ids::MessageId, message::Role},
    };
    use std::sync::Mutex;

    fn guarded_pending_stream() -> (GuardedEventStream, MessageId) {
        let guard = Arc::new(Mutex::new(LiveStreamGuard::default()));
        let message_id = MessageId::random();
        let started = Event::TextMessageStart(TextMessageStartEvent {
            base: BaseEvent {
                timestamp: None,
                raw_event: None,
            },
            message_id: message_id.clone(),
            role: Role::Assistant,
        });
        let guard_for_stream = guard.clone();
        let source = tokio_stream::StreamExt::chain(
            tokio_stream::once(started),
            futures_util::stream::pending(),
        );
        let stream = tokio_stream::StreamExt::map(source, move |event| {
            frame_guarded_live_event(
                event,
                &mut guard_for_stream.lock().expect("live stream guard"),
            )
            .expect("event should frame")
        });
        (
            GuardedEventStream {
                stream: Box::pin(stream),
                guard,
            },
            message_id,
        )
    }

    #[test]
    fn terminal_frame_detection_uses_top_level_event_type() {
        let text = Bytes::from(
            "data: {\"type\":\"TEXT_MESSAGE_CONTENT\",\"delta\":\"quoted \\\"type\\\":\\\"RUN_ERROR\\\" text\"}\n\n",
        );
        assert!(!is_run_terminal_frame(&text));

        let terminal = Bytes::from(
            "data: {\"type\":\"TEXT_MESSAGE_END\"}\n\ndata: {\"type\":\"RUN_FINISHED\"}\n\n",
        );
        assert!(is_run_terminal_frame(&terminal));
    }

    #[tokio::test]
    async fn shutdown_waits_for_jitter_then_finalizes_and_errors_stream() {
        let shutdown = ServeShutdown::new(
            StreamDrainConfig::new(Duration::from_millis(30), Duration::from_millis(30)).unwrap(),
        );
        let (guarded, message_id) = guarded_pending_stream();
        let mut stream = close_stream_on_shutdown(guarded, shutdown.clone(), "thread", "run");
        let first = tokio_stream::StreamExt::next(&mut stream)
            .await
            .expect("text start");
        assert!(first
            .as_ref()
            .windows(18)
            .any(|window| window == b"TEXT_MESSAGE_START"));

        shutdown.begin(Duration::from_secs(1));
        assert!(
            tokio::time::timeout(
                Duration::from_millis(10),
                tokio_stream::StreamExt::next(&mut stream)
            )
            .await
            .is_err(),
            "observing shutdown must not close the stream before its jitter"
        );
        let terminal = tokio::time::timeout(
            Duration::from_millis(200),
            tokio_stream::StreamExt::next(&mut stream),
        )
        .await
        .expect("jittered close deadline")
        .expect("terminal frame");
        let terminal = std::str::from_utf8(&terminal).expect("utf8 terminal frames");
        let end = terminal.find("TEXT_MESSAGE_END").expect("lifecycle end");
        let error = terminal.find("RUN_ERROR").expect("run error");
        assert!(end < error, "lifecycle must close before RUN_ERROR");
        assert!(terminal.contains(&message_id.to_string()));
        assert!(tokio_stream::StreamExt::next(&mut stream).await.is_none());
    }

    #[tokio::test]
    async fn stream_jitter_is_clamped_to_remaining_drain_budget() {
        let shutdown = ServeShutdown::new(
            StreamDrainConfig::new(Duration::from_secs(1), Duration::from_secs(1)).unwrap(),
        );
        let guarded = GuardedEventStream {
            stream: Box::pin(futures_util::stream::pending()),
            guard: Arc::new(Mutex::new(LiveStreamGuard::default())),
        };
        let mut stream = close_stream_on_shutdown(guarded, shutdown.clone(), "thread", "run");

        shutdown.begin(Duration::from_millis(20));
        let terminal = tokio::time::timeout(
            Duration::from_millis(150),
            tokio_stream::StreamExt::next(&mut stream),
        )
        .await
        .expect("remaining drain budget must cap jitter")
        .expect("RUN_ERROR frame");
        assert!(std::str::from_utf8(&terminal)
            .expect("utf8 RUN_ERROR")
            .contains("RUN_ERROR"));
    }

    use super::*;

    #[test]
    fn jitter_config_rejects_zero_and_inverted_bounds() {
        assert!(StreamDrainConfig::new(Duration::ZERO, Duration::from_secs(1)).is_err());
        assert!(StreamDrainConfig::new(Duration::from_secs(2), Duration::from_secs(1)).is_err());
    }

    #[test]
    fn streams_sample_jitter_independently() {
        let shutdown = ServeShutdown::new(
            StreamDrainConfig::new(Duration::from_millis(10), Duration::from_millis(50)).unwrap(),
        );
        shutdown.begin(Duration::from_secs(1));
        let samples = (0..32)
            .map(|_| shutdown.sample_stream_delay())
            .collect::<std::collections::HashSet<_>>();
        assert!(
            samples.len() > 1,
            "independent samples should produce a spread"
        );
    }

    #[tokio::test]
    async fn sampled_delay_is_clamped_to_remaining_budget() {
        let shutdown = ServeShutdown::new(
            StreamDrainConfig::new(Duration::from_secs(20), Duration::from_secs(20)).unwrap(),
        );
        shutdown.begin(Duration::from_millis(30));
        shutdown.cancelled().await;
        assert!(shutdown.sample_stream_delay() <= Duration::from_millis(30));
    }
}

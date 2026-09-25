use anyhow::Context;
use harnx_toolset::{
    ProgressChunk, ProgressMessage, ToolProgress, ToolProgressHandle, ToolProgressPatch,
    ToolRequest, CAPABILITY_TOOL_PROGRESS,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

const TOOL_PROGRESS_COALESCE_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Default)]
struct ProgressState {
    finished: bool,
    revision: u64,
    snapshot: ToolProgressPatch,
}

struct NatsToolProgress {
    state: Mutex<ProgressState>,
    wake: mpsc::Sender<()>,
}

impl ToolProgress for NatsToolProgress {
    fn update(&self, patch: ToolProgressPatch) {
        let patch = patch.bounded();
        if patch.is_empty() {
            return;
        }
        let mut state = self.state.lock().expect("tool progress state poisoned");
        if state.finished {
            return;
        }
        state.snapshot.merge(patch);
        state.revision = state.revision.wrapping_add(1);
        let _ = self.wake.try_send(());
    }
}

/// Per-call progress publisher retained by the request path until the reply is built.
pub(super) struct ProgressPublisher {
    sink: Arc<NatsToolProgress>,
    done: oneshot::Receiver<()>,
}

impl ProgressPublisher {
    pub(super) fn for_request(
        client: &async_nats::Client,
        subject: String,
        request: &ToolRequest,
    ) -> Option<Self> {
        if !supports_progress(request) {
            return None;
        }
        let (wake, receiver) = mpsc::channel(1);
        let (finished, done) = oneshot::channel();
        let sink = Arc::new(NatsToolProgress {
            state: Mutex::new(ProgressState::default()),
            wake,
        });
        tokio::spawn(run_publisher(
            client.clone(),
            subject,
            request.call_id.clone(),
            sink.clone(),
            receiver,
            finished,
        ));
        Some(Self { sink, done })
    }

    pub(super) fn handle(&self) -> ToolProgressHandle {
        ToolProgressHandle::new(self.sink.clone())
    }

    pub(super) async fn finish(self) -> Option<ToolProgressPatch> {
        let snapshot = {
            let mut state = self
                .sink
                .state
                .lock()
                .expect("tool progress state poisoned");
            state.finished = true;
            (!state.snapshot.is_empty()).then(|| state.snapshot.clone())
        };
        let _ = self.sink.wake.try_send(());
        let _ = self.done.await;
        snapshot
    }
}

fn supports_progress(request: &ToolRequest) -> bool {
    request.capabilities.contains(CAPABILITY_TOOL_PROGRESS)
}

async fn run_publisher(
    client: async_nats::Client,
    subject: String,
    call_id: String,
    sink: Arc<NatsToolProgress>,
    mut wake: mpsc::Receiver<()>,
    finished: oneshot::Sender<()>,
) {
    let mut published_revision = 0;
    let mut next_publish = Instant::now();
    while wake.recv().await.is_some() {
        let state = state_snapshot(&sink);
        if state.revision != published_revision {
            tokio::time::sleep_until(next_publish).await;
            let latest = state_snapshot(&sink);
            publish_snapshot(&client, &subject, &call_id, latest.snapshot).await;
            published_revision = latest.revision;
            next_publish = Instant::now() + TOOL_PROGRESS_COALESCE_INTERVAL;
        }
        let latest = state_snapshot(&sink);
        if latest.finished {
            if latest.revision != published_revision {
                publish_snapshot(&client, &subject, &call_id, latest.snapshot).await;
            }
            let _ = client.flush().await;
            let _ = finished.send(());
            return;
        }
        if latest.revision != published_revision {
            let _ = sink.wake.try_send(());
        }
    }
}

#[derive(Clone)]
struct StateSnapshot {
    finished: bool,
    revision: u64,
    snapshot: ToolProgressPatch,
}

fn state_snapshot(sink: &NatsToolProgress) -> StateSnapshot {
    let state = sink.state.lock().expect("tool progress state poisoned");
    StateSnapshot {
        finished: state.finished,
        revision: state.revision,
        snapshot: state.snapshot.clone(),
    }
}

async fn publish_snapshot(
    client: &async_nats::Client,
    subject: &str,
    call_id: &str,
    snapshot: ToolProgressPatch,
) {
    if snapshot.is_empty() {
        return;
    }
    let message = ProgressMessage {
        call_id: call_id.to_string(),
        chunk: ProgressChunk::V1(snapshot),
    };
    let result = async {
        let payload = serde_json::to_vec(&message).context("encode tool progress")?;
        client
            .publish(subject.to_string(), payload.into())
            .await
            .context("publish tool progress")
    }
    .await;
    if let Err(error) = result {
        log::warn!("tool progress update dropped: {error:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn request(capabilities: BTreeSet<String>) -> ToolRequest {
        ToolRequest {
            replay: None,
            operation_id: "operation-1".into(),
            call_id: "call-1".into(),
            tool: "echo".into(),
            args: serde_json::json!({}),
            parent_session_id: None,
            tool_call_id: None,
            capabilities,
        }
    }

    #[test]
    fn publisher_requires_negotiated_capability() {
        assert!(!supports_progress(&request(BTreeSet::new())));
        assert!(supports_progress(&request(BTreeSet::from([
            CAPABILITY_TOOL_PROGRESS.to_string()
        ]))));
    }

    #[test]
    fn progress_state_defaults_to_empty_and_open() {
        let state = ProgressState::default();
        assert!(!state.finished);
        assert_eq!(state.revision, 0);
        assert!(state.snapshot.is_empty());
    }
}

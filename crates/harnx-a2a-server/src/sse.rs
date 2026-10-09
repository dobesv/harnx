//! Stream sequence filtering sits below the upstream ProtoJSON/JSON-RPC SSE codec.
use crate::{runner::A2aEvent, store::TaskRecord};
use a2a_lf::{A2AError, StreamResponse};
use axum::{extract::Request, middleware::Next, response::Response};
use futures::{stream, stream::BoxStream, StreamExt};
use tokio::sync::broadcast;

pub(crate) fn task_stream(
    snapshot: TaskRecord,
    events: Option<broadcast::Receiver<A2aEvent>>,
) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
    let terminal = snapshot.task.status.state.is_terminal();
    let sequence = snapshot.stream_seq;
    let first = stream::once(async move { Ok(StreamResponse::Task(snapshot.task)) });
    let updates = stream::unfold(
        (events, sequence, terminal),
        |(events, mut sequence, done)| async move {
            if done {
                return None;
            }
            let mut events = events?;
            loop {
                match events.recv().await {
                    Ok(event) if event.sequence <= sequence => continue,
                    Ok(event) => {
                        sequence = event.sequence;
                        let terminal = event.is_terminal();
                        return Some((Ok(event.response), (Some(events), sequence, terminal)));
                    }
                    // A bounded receiver cannot recover its lost deltas. Disconnect
                    // only this client; it can reconnect using a fresh snapshot.
                    Err(_) => {
                        return Some((
                            Err(A2AError::internal("task stream interrupted; reconnect")),
                            (None, sequence, true),
                        ))
                    }
                }
            }
        },
    );
    first.chain(updates).boxed()
}

/// Upstream supplies text/event-stream, no-cache, and idle comments every 15s.
/// Disable nginx buffering only on SSE responses, not discovery or RPC errors.
pub(crate) async fn headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    if response
        .headers()
        .get("content-type")
        .is_some_and(|value| value == "text/event-stream")
    {
        response
            .headers_mut()
            .insert("x-accel-buffering", "no".parse().unwrap());
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2a_lf::{Task, TaskState, TaskStatus, TaskStatusUpdateEvent};

    fn snapshot(state: TaskState) -> TaskRecord {
        harnx_core::require_nextest();
        TaskRecord {
            version: 1,
            task: Task {
                id: "task".into(),
                context_id: "context".into(),
                status: TaskStatus {
                    state,
                    message: None,
                    timestamp: None,
                },
                artifacts: None,
                history: None,
                metadata: None,
            },
            user_msg_id: String::new(),
            user_msg_seq: 0,
            execution_id: String::new(),
            revision: 2,
            stream_seq: 5,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }
    fn event(sequence: u64, state: TaskState) -> A2aEvent {
        A2aEvent {
            sequence,
            response: StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                task_id: "task".into(),
                context_id: "context".into(),
                status: TaskStatus {
                    state,
                    message: None,
                    timestamp: None,
                },
                metadata: None,
            }),
        }
    }
    #[tokio::test]
    async fn filters_snapshot_sequences_and_closes_at_terminal() {
        let (tx, rx) = broadcast::channel(16);
        for sequence in [4, 5, 6, 6] {
            tx.send(event(sequence, TaskState::Working)).unwrap();
        }
        tx.send(event(7, TaskState::Completed)).unwrap();
        tx.send(event(8, TaskState::Working)).unwrap();
        let mut stream = task_stream(snapshot(TaskState::Working), Some(rx));
        assert!(matches!(
            stream.next().await.unwrap().unwrap(),
            StreamResponse::Task(_)
        ));
        assert!(
            matches!(stream.next().await.unwrap().unwrap(), StreamResponse::StatusUpdate(update) if update.status.state == TaskState::Working)
        );
        assert!(
            matches!(stream.next().await.unwrap().unwrap(), StreamResponse::StatusUpdate(update) if update.status.state == TaskState::Completed)
        );
        assert!(stream.next().await.is_none());
    }
    #[tokio::test]
    async fn lag_disconnects_only_its_receiver() {
        let (tx, rx) = broadcast::channel(2);
        let mut stream = task_stream(snapshot(TaskState::Working), Some(rx));
        stream.next().await.unwrap().unwrap();
        for sequence in 6..10 {
            tx.send(event(sequence, TaskState::Working)).unwrap();
        }
        assert!(stream.next().await.unwrap().is_err());
        assert!(stream.next().await.is_none());
        let mut healthy = tx.subscribe();
        tx.send(event(10, TaskState::Completed)).unwrap();
        assert!(healthy.recv().await.unwrap().is_terminal());
    }
    #[tokio::test]
    async fn terminal_snapshot_closes_even_with_live_sender() {
        let (_tx, rx) = broadcast::channel(2);
        let mut stream = task_stream(snapshot(TaskState::Completed), Some(rx));
        assert!(matches!(
            stream.next().await.unwrap().unwrap(),
            StreamResponse::Task(_)
        ));
        assert!(stream.next().await.is_none());
    }
}

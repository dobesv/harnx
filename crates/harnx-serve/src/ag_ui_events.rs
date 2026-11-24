//! AG-UI event serialization, stream framing, and history formatting.
#![allow(dead_code)]

use ag_ui_core::{event::Event, types::message::Role};
use bytes::Bytes;
use harnx_core::message::MessageRole;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;

use crate::ag_ui::{AgUiError, SharedLiveStreamGuard};
use crate::ag_ui_lifecycle::LiveStreamGuard;
use crate::ag_ui_sync::frame_run_error_event;
use crate::session_actor::{SessionCommand, SessionHandle};
use crate::transcript_snapshot::{HistorySnapshot, SnapshotFeatures};

#[derive(Clone, Copy)]
pub(crate) struct LiveEventContext<'a> {
    pub thread_id: &'a str,
    pub run_id: &'a str,
}

pub fn frame_event(event: &Event) -> Result<String, AgUiError> {
    let json = serde_json::to_string(event)
        .map_err(|err| AgUiError::Internal(format!("failed to serialize AG-UI event: {err}")))?;
    Ok(format!("data: {json}\n\n"))
}

pub(crate) fn append_event_frames(initial: Option<Bytes>, events: Vec<Event>) -> Option<Bytes> {
    let appended = events
        .into_iter()
        .filter_map(|event| frame_event(&event).ok())
        .collect::<String>();
    if appended.is_empty() {
        return initial;
    }
    let mut frames = initial.map_or_else(Vec::new, |bytes| bytes.to_vec());
    frames.extend_from_slice(appended.as_bytes());
    Some(Bytes::from(frames))
}

pub(crate) fn live_subscription_events(
    handle: &SessionHandle,
    events: broadcast::Receiver<Event>,
    features: SnapshotFeatures,
) -> impl tokio_stream::Stream<Item = Event> {
    let handle = handle.clone();
    let events = tokio_stream::StreamExt::then(BroadcastStream::new(events), move |item| {
        let handle = handle.clone();
        async move {
            match item {
                Ok(event) => features.expand_live_event(event).collect(),
                Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(_)) => {
                    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                    handle
                        .tx
                        .send(SessionCommand::Get { reply: reply_tx })
                        .await
                        .expect("send get");
                    let info = reply_rx.await.expect("recv get");
                    HistorySnapshot {
                        messages: info.history_snapshot,
                        reasoning: info.history_reasoning,
                        compaction: info.compaction,
                    }
                    .events_for(features)
                }
            }
        }
    });
    futures_util::StreamExt::flat_map(events, tokio_stream::iter)
}

/// Whether local run state blocks idle or remote-follow stream handling.
pub(crate) fn session_state_is_active(state: &crate::session_actor::SessionState) -> bool {
    matches!(
        state,
        crate::session_actor::SessionState::Running { .. }
            | crate::session_actor::SessionState::AwaitingApproval { .. }
    )
}

#[derive(Clone, Copy)]
pub(crate) enum FirstRunState {
    AwaitingStarted,
    Active,
    Complete,
    Errored,
}

pub(crate) fn frame_run_finished_event(
    thread_id: &str,
    run_id: &str,
    result: Option<ag_ui_core::JsonValue>,
) -> Bytes {
    let mut body = serde_json::json!({
        "type": "RUN_FINISHED",
        "threadId": thread_id,
        "runId": run_id,
    });
    if let Some(result) = result {
        if let Some(outcome) = result.get("outcome") {
            body["outcome"] = outcome.clone();
        } else {
            body["result"] = result;
        }
    }
    Bytes::from(format!("data: {body}\n\n"))
}

pub(crate) fn frame_terminal_after_lifecycle_closes(
    guard: &mut LiveStreamGuard,
    terminal: Bytes,
) -> Bytes {
    let closes = guard.finalize_open_lifecycles();
    if closes.is_empty() {
        return terminal;
    }
    let mut frames = Vec::with_capacity(closes.len() + terminal.len());
    frames.extend_from_slice(&closes);
    frames.extend_from_slice(&terminal);
    Bytes::from(frames)
}

pub(crate) fn frame_guarded_live_event(event: Event, guard: &mut LiveStreamGuard) -> Option<Bytes> {
    guard.frame_event(event)
}

pub(crate) fn frame_live_event(
    event: Event,
    state: &mut FirstRunState,
    guard: &mut LiveStreamGuard,
    context: LiveEventContext<'_>,
) -> Option<Bytes> {
    match *state {
        FirstRunState::AwaitingStarted => match event {
            Event::RunStarted(_) => {
                *state = FirstRunState::Active;
                None
            }
            Event::RunFinished(event) => {
                *state = FirstRunState::Complete;
                Some(frame_terminal_after_lifecycle_closes(
                    guard,
                    frame_run_finished_event(context.thread_id, context.run_id, event.result),
                ))
            }
            Event::RunError(err) => {
                *state = FirstRunState::Errored;
                Some(frame_terminal_after_lifecycle_closes(
                    guard,
                    Bytes::from(frame_run_error_event(
                        context.thread_id,
                        context.run_id,
                        &err.message,
                    )),
                ))
            }
            other => frame_guarded_live_event(other, guard),
        },
        FirstRunState::Active => match event {
            Event::RunStarted(_) => None,
            Event::RunFinished(event) => {
                *state = FirstRunState::Complete;
                Some(frame_terminal_after_lifecycle_closes(
                    guard,
                    frame_run_finished_event(context.thread_id, context.run_id, event.result),
                ))
            }
            Event::RunError(err) => {
                *state = FirstRunState::Errored;
                Some(frame_terminal_after_lifecycle_closes(
                    guard,
                    Bytes::from(frame_run_error_event(
                        context.thread_id,
                        context.run_id,
                        &err.message,
                    )),
                ))
            }
            other => frame_guarded_live_event(other, guard),
        },
        FirstRunState::Complete | FirstRunState::Errored => None,
    }
}

pub(crate) fn build_live_event_body(
    context: LiveEventContext<'_>,
    snapshot_frame: Option<Bytes>,
    live_stream: impl tokio_stream::Stream<Item = Event> + Send + Sync + 'static,
    guard: SharedLiveStreamGuard,
) -> impl tokio_stream::Stream<Item = Bytes> + Send + Sync + 'static {
    let run_id = context.run_id.to_string();
    let thread_id = context.thread_id.to_string();
    let terminal_frame = std::sync::Arc::new(std::sync::Mutex::new(None));
    let live_stream = {
        let mut state = FirstRunState::AwaitingStarted;
        let terminal_frame = terminal_frame.clone();
        let framed = tokio_stream::StreamExt::map(live_stream, move |event| {
            let is_terminal = matches!(event, Event::RunFinished(_) | Event::RunError(_));
            let bytes = frame_live_event(
                event,
                &mut state,
                &mut guard.lock().expect("live stream guard"),
                LiveEventContext {
                    thread_id: &thread_id,
                    run_id: &run_id,
                },
            );
            (bytes, is_terminal)
        });
        let body = tokio_stream::StreamExt::take_while(framed, move |(bytes, is_terminal)| {
            if *is_terminal {
                *terminal_frame.lock().expect("terminal frame lock") = bytes.clone();
                false
            } else {
                true
            }
        });
        tokio_stream::StreamExt::filter_map(body, |(bytes, _)| bytes)
    };
    let terminal_stream = {
        let terminal_frame = terminal_frame.clone();
        tokio_stream::StreamExt::filter_map(tokio_stream::once(()), move |_| {
            terminal_frame.lock().expect("terminal frame lock").take()
        })
    };
    let live_stream = tokio_stream::StreamExt::chain(live_stream, terminal_stream);
    tokio_stream::StreamExt::chain(tokio_stream::iter(snapshot_frame), live_stream)
}

pub(crate) fn ag_ui_role_for_history(role: MessageRole) -> Role {
    match role {
        MessageRole::System => Role::System,
        MessageRole::Assistant => Role::Assistant,
        MessageRole::User => Role::User,
        MessageRole::Tool => Role::Tool,
    }
}

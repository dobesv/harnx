//! Shared committed event transport. Conditional publication never rebases.
use super::{ContextSnapshot, PendingEvent};
use anyhow::{ensure, Context, Result};
use async_nats::jetstream::{self, message::PublishMessage, stream::LastRawMessageErrorKind};

pub(crate) async fn publish_pending(
    store: &super::super::A2aStore,
    context: &ContextSnapshot,
    storage: &str,
) -> Result<u64> {
    let active = context
        .document
        .state
        .active
        .as_ref()
        .context("no active event task")?;
    let pending = active
        .publication
        .pending
        .as_ref()
        .context("no pending event")?;
    let (_, id) = super::super::parse_task_id(&active.snapshot.task.id)?;
    let subject = format!("a2a.tasks.{storage}.{id}");
    let stream = ensure_stream(store).await?;
    // Resolution before retry also works after JetStream's finite dedupe window.
    if let Some(seq) = matching_tail(&stream, &subject, pending).await? {
        return Ok(seq);
    }
    if let Some(since) = pending.committed_at {
        crate::diagnostics::age("outbox", since);
    }
    let result = publish_committed(store, &stream, &subject, pending).await;
    crate::diagnostics::outcome("publish", result.is_ok());
    result
}
pub(crate) fn task_subject(storage: &str, task_id: &str) -> Result<String> {
    let (_, id) = super::super::parse_task_id(task_id)?;
    Ok(format!("a2a.tasks.{storage}.{id}"))
}

pub(crate) async fn ensure_stream(
    store: &super::super::A2aStore,
) -> Result<jetstream::stream::Stream> {
    harnx_runtime::a2a_events::ensure(store.metadata().jetstream(), store.metadata().replicas())
        .await
}

async fn matching_tail(
    stream: &jetstream::stream::Stream,
    subject: &str,
    pending: &PendingEvent,
) -> Result<Option<u64>> {
    match stream.get_last_raw_message_by_subject(subject).await {
        Ok(raw) => {
            if raw.sequence == pending.expected_subject_sequence {
                return Ok(None);
            }
            // A successor may already have cleared this event and committed
            // another. Inspect the first retained successor, not a newer tail.
            let raw = stream
                .raw_message_builder()
                .sequence(pending.expected_subject_sequence + 1)
                .next_by_subject(subject)
                .send()
                .await?;
            let saved: PendingEvent = serde_json::from_slice(&raw.payload)?;
            ensure!(
                serde_json::to_value(saved)? == serde_json::to_value(pending)?,
                "task event predecessor or identity conflict"
            );
            Ok(Some(raw.sequence))
        }
        Err(error) if error.kind() == LastRawMessageErrorKind::NoMessageFound => {
            ensure!(
                pending.expected_subject_sequence == 0,
                "task event predecessor missing"
            );
            Ok(None)
        }
        Err(error) => Err(error.into()),
    }
}

async fn publish_committed(
    store: &super::super::A2aStore,
    stream: &jetstream::stream::Stream,
    subject: &str,
    pending: &PendingEvent,
) -> Result<u64> {
    let js = store.metadata().jetstream();
    let bytes = serde_json::to_vec(pending)?;
    let result = js
        .send_publish(
            subject.to_owned(),
            PublishMessage::build()
                .message_id(pending.commit_id.clone())
                .expected_last_subject_sequence(pending.expected_subject_sequence)
                .payload(bytes.into()),
        )
        .await;
    let ack = match result {
        Ok(ack) => ack.await.map_err(anyhow::Error::from),
        Err(error) => Err(error.into()),
    };
    #[cfg(feature = "fault-injection")]
    let ack = suppress_ack(store, pending, ack);
    #[cfg(feature = "fault-injection")]
    if ack.is_ok() {
        store
            .context_hooks
            .checkpoint(crate::fault_injection::Boundary::EventPublished)
            .await;
    }
    let resolved = matching_tail(stream, subject, pending).await?;
    match resolved {
        Some(seq) => Ok(seq),
        None => {
            ack.context("task event publish unconfirmed")?;
            anyhow::bail!("task event identity missing after acknowledgement")
        }
    }
}

#[cfg(feature = "fault-injection")]
fn suppress_ack(
    store: &super::super::A2aStore,
    pending: &PendingEvent,
    ack: Result<jetstream::publish::PublishAck>,
) -> Result<jetstream::publish::PublishAck> {
    let ack = ack?;
    let terminal = matches!(&pending.response, a2a_lf::StreamResponse::StatusUpdate(event) if event.status.state.is_terminal());
    let lost = store.context_hooks.take_event_ack_loss()
        || (terminal && store.context_hooks.take_terminal_ack_loss());
    if lost {
        Err(anyhow::anyhow!("injected event acknowledgement loss"))
    } else {
        Ok(ack)
    }
}

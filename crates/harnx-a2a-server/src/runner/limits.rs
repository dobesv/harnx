//! Leave room for input, authority fields and four serialized answer copies.
use super::*;

pub(super) const OUTPUT_LIMIT_MESSAGE: &str = "task output exceeds A2A payload budget";

pub(super) fn output_limit(payload: usize) -> usize {
    let payload = if payload == 0 { 1024 * 1024 } else { payload };
    payload.saturating_sub(128 * 1024) / 4
}

pub(super) async fn task_output_limit(
    store: &A2aStore,
    context: &crate::store::context::ContextSnapshot,
) -> Result<usize> {
    let payload = store.metadata().a2a_payload_limit().await?;
    let mut base = context.document.clone();
    if let Some(active) = base.state.active.as_mut() {
        active.snapshot.task.artifacts = None;
        active.snapshot.task.status.message = None;
        if let Some(history) = active.snapshot.task.history.as_mut() {
            history.retain(|message| message.role != a2a_lf::Role::Agent);
        }
        active.publication.pending = None;
    }
    let base_bytes = serde_json::to_vec(&base)?.len();
    Ok(output_limit(payload).min(payload.saturating_sub(base_bytes + 64 * 1024) / 4))
}

pub(super) fn bounded_outcome(
    outcome: (TaskState, Option<String>),
    limit: usize,
) -> (TaskState, Option<String>) {
    let fits = outcome
        .1
        .as_ref()
        .is_none_or(|text| encoded_len(text) <= limit);
    if fits {
        outcome
    } else {
        (TaskState::Failed, Some(OUTPUT_LIMIT_MESSAGE.into()))
    }
}

pub(super) fn encoded_len(text: &str) -> usize {
    // String serialization can't fail. Exclude its two quote bytes.
    serde_json::to_string(text)
        .expect("string JSON serialization")
        .len()
        - 2
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn output_budget_counts_json_escapes_and_maps_large_completion_to_explicit_failure() {
        assert_eq!(output_limit(1024 * 1024), 224 * 1024);
        assert_eq!(output_limit(0), 224 * 1024);
        assert_eq!(encoded_len("\n\u{1}"), 8);
        let accepted = (TaskState::Completed, Some("x".repeat(224 * 1024)));
        assert_eq!(bounded_outcome(accepted.clone(), 224 * 1024), accepted);
        assert_eq!(
            bounded_outcome(
                (TaskState::Completed, Some("x".repeat(224 * 1024 + 1))),
                224 * 1024
            ),
            (TaskState::Failed, Some(OUTPUT_LIMIT_MESSAGE.into()))
        );
        let mut output = super::super::event_map::Output::default();
        assert!(output.accept("\u{1}".repeat(100), 100).is_err());
        assert!(output.text.is_empty());
    }
}

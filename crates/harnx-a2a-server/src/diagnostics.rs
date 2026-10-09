//! Bounded-label observations. No session, task, principal or subject labels.
use crate::store::context::{AdmissionPhase, ContextSnapshot};
use chrono::{DateTime, Utc};

pub(crate) fn age(phase: &'static str, since: DateTime<Utc>) {
    let seconds = (Utc::now() - since).num_milliseconds().max(0) as f64 / 1000.0;
    metrics::histogram!("harnx_a2a_pending_age_seconds", "phase" => phase).record(seconds);
}

pub(crate) fn pending(context: &ContextSnapshot) {
    let Some(active) = &context.document.state.active else {
        return;
    };
    if active.admission.phase == AdmissionPhase::Reserved {
        age("admission", active.snapshot.created_at);
    }
    if let Some(cancel) = &active.cancel {
        if !active.stop_confirmed {
            age(
                "cancel",
                cancel.requested_at.unwrap_or(active.snapshot.created_at),
            );
        }
    }
    if let Some(event) = &active.publication.pending {
        age(
            "outbox",
            event.committed_at.unwrap_or(active.snapshot.updated_at),
        );
    }
}

pub(crate) fn outcome(operation: &'static str, success: bool) {
    metrics::counter!("harnx_a2a_operations_total", "op" => operation, "outcome" => if success { "ok" } else { "error" }).increment(1);
}

#[cfg(test)]
mod tests {
    #[test]
    fn diagnostic_labels_are_bounded_and_ages_nonnegative() {
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshot = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || {
            super::age("outbox", chrono::Utc::now() + chrono::Duration::seconds(1));
            super::outcome("publish", false);
        });
        let values = snapshot.snapshot().into_vec();
        assert_eq!(values.len(), 2);
        for (key, _, _, value) in values {
            let labels: Vec<_> = key
                .key()
                .labels()
                .map(|label| (label.key(), label.value()))
                .collect();
            if key.key().name() == "harnx_a2a_pending_age_seconds" {
                assert_eq!(labels, [("phase", "outbox")]);
                assert_eq!(
                    value,
                    metrics_util::debugging::DebugValue::Histogram(vec![0.0.into()])
                );
            } else {
                assert_eq!(labels, [("op", "publish"), ("outcome", "error")]);
                assert_eq!(value, metrics_util::debugging::DebugValue::Counter(1));
            }
        }
    }
}

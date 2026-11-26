//! Read-only A2A hold predicate, shared by session storage stats and the
//! manual expired-session cleanup preview (T2).
//!
//! It mirrors the decisions of [`super::fence`] and
//! [`super::state::ensure_first_settled`] without taking the scoped A2A lease
//! and without mutating or purging anything. Input is key lookup facts —
//! `(operation, payload)` per key, keeping absent distinct from a Delete/Purge
//! marker — so the same decision works over a bulk snapshot and over targeted
//! leader reads. Real fences stay authoritative: a session that passes this
//! predicate can still be refused by a later live fence.
use super::state::context_unresolved;
use anyhow::{Context as _, Result};
use async_nats::jetstream::kv::{self, Operation};
use bytes::Bytes;
use harnx_core::crypto::sha256;
use serde_json::Value;

/// One A2A key's latest observed state. `Absent` is a key the reader found
/// nothing under; `Marker` is a Delete or Purge marker; `Live` is a Put value.
/// Registry, reservation and archive markers count as absent, the way the
/// fence's leader reads treat them; only the context distinguishes all three.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordFact {
    Absent,
    Marker(Operation),
    Live(Bytes),
}

impl RecordFact {
    /// From a leader (or snapshot) entry, retaining the marker state.
    pub fn from_entry(entry: Option<&kv::Entry>) -> Self {
        match entry {
            None => Self::Absent,
            Some(entry) if entry.operation == Operation::Put => Self::Live(entry.value.clone()),
            Some(entry) => Self::Marker(entry.operation),
        }
    }

    /// From a `leader_value` read, which folds markers into absence — matching
    /// how the fence reads registry, reservation and archive keys.
    pub fn from_leader_value(value: Option<Bytes>) -> Self {
        value.map(Self::Live).unwrap_or(Self::Absent)
    }

    fn live(&self) -> bool {
        matches!(self, Self::Live(_))
    }
}

/// Lookup facts for one session's A2A admission state. Callers fetch exactly
/// the keys [`context_key`] and [`registry_key`] name, then the reservation
/// key [`reservation_key_from_registry`] derives, then the archive key
/// [`archive_key_when_inspected`] derives; facts they could not derive are
/// [`RecordFact::Absent`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct A2aHoldInputs {
    pub context: RecordFact,
    pub registry: RecordFact,
    pub reservation: RecordFact,
    pub archive: RecordFact,
}

impl A2aHoldInputs {
    /// Facts for a session with no A2A state observed at all.
    pub fn absent() -> Self {
        Self {
            context: RecordFact::Absent,
            registry: RecordFact::Absent,
            reservation: RecordFact::Absent,
            archive: RecordFact::Absent,
        }
    }
}

/// Which read-only A2A holds one session has. Expected unresolved or unsettled
/// state is a hold, not an error; malformed state and transport failures are
/// errors and never a false no-hold.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct A2aHoldSummary {
    /// The context holds unresolved stop, projections, publication or
    /// outbox state.
    pub unresolved_context: bool,
    /// The session's first-message reservation is live, allocated here, and
    /// its task has no live archive record.
    pub unsettled_first_reservation: bool,
}

impl A2aHoldSummary {
    pub fn any(&self) -> bool {
        self.unresolved_context || self.unsettled_first_reservation
    }
}

/// `sessions/<storage_key>/a2a/context`
pub fn context_key(storage: &str) -> String {
    format!("sessions/{storage}/a2a/context")
}

/// `a2a.registry.<sha256(storage_key)>`
pub fn registry_key(storage: &str) -> String {
    format!("a2a.registry.{}", sha256(storage))
}

/// The shared first-message reservation key a live registry record's first
/// identity derives, mirroring the fence's derivation. `Ok(None)` when the
/// registry holds no live identity to derive a reservation from.
pub fn reservation_key_from_registry(registry: &RecordFact) -> Result<Option<String>> {
    let RecordFact::Live(payload) = registry else {
        return Ok(None);
    };
    let doc: Value = serde_json::from_slice(payload).context("invalid A2A registry record")?;
    let Some(identity) = doc.pointer("/first/identity") else {
        return Ok(None);
    };
    Ok(Some(reservation_key_for_identity(identity)?))
}

/// `a2a/first-messages/<sha256(identity)>`, the shared first-message
/// reservation key, from a live registry first identity.
fn reservation_key_for_identity(identity: &Value) -> Result<String> {
    let identity = serde_json::to_string(identity)?;
    Ok(format!("a2a/first-messages/{}", sha256(&identity)))
}

/// The reservation record the fence would inspect, parsed. `Ok(None)` when
/// the first-reservation rule cannot hold for these facts: the registry holds
/// no live first identity, the reservation is not a live record, or it is
/// allocated to another session. The payload is parsed only where the fence
/// would parse it too, so a reservation the fence never looks at cannot fail
/// the decision.
fn inspectable_reservation(
    storage: &str,
    registry_doc: Option<&Value>,
    reservation: &RecordFact,
) -> Result<Option<Value>> {
    if registry_doc
        .and_then(|doc| doc.pointer("/first/identity"))
        .is_none()
    {
        return Ok(None);
    }
    let RecordFact::Live(payload) = reservation else {
        return Ok(None);
    };
    let first: Value =
        serde_json::from_slice(payload).context("invalid A2A first-message reservation")?;
    if first
        .pointer("/allocation/storage_key")
        .and_then(Value::as_str)
        != Some(storage)
    {
        return Ok(None);
    }
    Ok(Some(first))
}

/// The first-message reservation key the fence would read for these facts,
/// derived in the fence's own order. `Ok(None)` when a context marker skips
/// the first-reservation rule, the context holds the session as unresolved
/// before the reservation is ever read, or the registry holds no live first
/// identity. Targeted readers use this so they never fetch a record the
/// fence skips.
pub fn reservation_key_when_inspected(
    context: &RecordFact,
    registry: &RecordFact,
) -> Result<Option<String>> {
    let Some((registry_doc, _)) = inspected_documents(context, registry)? else {
        return Ok(None);
    };
    let Some(identity) = registry_doc
        .as_ref()
        .and_then(|doc| doc.pointer("/first/identity"))
    else {
        return Ok(None);
    };
    Ok(Some(reservation_key_for_identity(identity)?))
}

/// The parsed registry and context documents for a context the fence does not
/// skip, in the fence's own order: registry parse, then context parse, then
/// the unresolved short-circuit. `Ok(None)` for a marker context or a context
/// whose unresolved state already holds the session — in both the fence never
/// reads the first-message reservation.
fn inspected_documents(
    context: &RecordFact,
    registry: &RecordFact,
) -> Result<Option<(Option<Value>, Option<Value>)>> {
    if matches!(context, RecordFact::Marker(_)) {
        return Ok(None);
    }
    let registry_doc = live_document(registry, "invalid A2A registry record")?;
    let context_doc = live_document(context, "invalid A2A context record")?;
    if let Some(doc) = &context_doc {
        if context_unresolved(doc)? {
            return Ok(None);
        }
    }
    Ok(Some((registry_doc, context_doc)))
}

/// The archive record key the fence would inspect, derived in the fence's own
/// order: registry parse, context parse, unresolved short-circuit, then the
/// reservation the fence reads, then the active-state settlement, and only
/// then the task identity. Callers fetch the archive record only when this
/// returns `Some`, so a malformed reservation payload or task identity cannot
/// error where the fence would not look. Malformed state the fence does
/// inspect is an error, the same one the fence rejects with.
pub fn archive_key_when_inspected(
    storage: &str,
    context: &RecordFact,
    registry: &RecordFact,
    reservation: &RecordFact,
) -> Result<Option<String>> {
    let Some((registry_doc, context_doc)) = inspected_documents(context, registry)? else {
        return Ok(None);
    };
    let Some(first) = inspectable_reservation(storage, registry_doc.as_ref(), reservation)? else {
        return Ok(None);
    };
    if active_state_settled(context_doc.as_ref()) {
        return Ok(None);
    }
    let (_, uuid) = reservation_task_uuid(&first)?;
    Ok(Some(format!("sessions/{storage}/a2a/archive/{uuid}")))
}

/// A non-null context active state — retired or not — satisfies the
/// first-reservation rule, as in the fence.
fn active_state_settled(context_doc: Option<&Value>) -> bool {
    context_doc.is_some_and(|doc| {
        doc.pointer("/state/active")
            .is_some_and(|active| !active.is_null())
    })
}

fn reservation_task_uuid(first: &Value) -> Result<(&str, &str)> {
    let task = first
        .pointer("/allocation/task_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("invalid A2A reservation task"))?;
    task.rsplit_once('.')
        .ok_or_else(|| anyhow::anyhow!("invalid A2A task identity"))
}

/// Decide which A2A holds the observed facts imply, mirroring the deletion
/// fence's own order: the registry record parses first, then the context; an
/// unresolved context holds the session and returns before the first-message
/// reservation is ever read, and a context marker skips the
/// first-reservation rule entirely. A context that is absent or a live Put is
/// held while its first-message reservation is allocated here with no live
/// archive record. A non-null active state — retired or not — satisfies the
/// first-reservation rule, as in the fence.
pub fn evaluate_a2a_holds(storage: &str, inputs: &A2aHoldInputs) -> Result<A2aHoldSummary> {
    // The fence parses the registry record before the context: malformed
    // registry state is an error even when the context would skip every
    // later check.
    let registry_doc = live_document(&inputs.registry, "invalid A2A registry record")?;
    let context_doc = live_document(&inputs.context, "invalid A2A context record")?;
    let mut summary = A2aHoldSummary::default();
    if let Some(doc) = &context_doc {
        if context_unresolved(doc)? {
            summary.unresolved_context = true;
            // The fence refuses an unresolved context before it ever reads
            // the first-message reservation; parse nothing further.
            return Ok(summary);
        }
    }
    if matches!(inputs.context, RecordFact::Marker(_)) {
        return Ok(summary);
    }
    if first_reservation_hold(storage, registry_doc.as_ref(), inputs, context_doc.as_ref())? {
        summary.unsettled_first_reservation = true;
    }
    Ok(summary)
}

fn live_document(fact: &RecordFact, error: &'static str) -> Result<Option<Value>> {
    match fact {
        RecordFact::Live(payload) => serde_json::from_slice(payload).context(error).map(Some),
        _ => Ok(None),
    }
}

fn first_reservation_hold(
    storage: &str,
    registry_doc: Option<&Value>,
    inputs: &A2aHoldInputs,
    context_doc: Option<&Value>,
) -> Result<bool> {
    let Some(first) = inspectable_reservation(storage, registry_doc, &inputs.reservation)? else {
        return Ok(false);
    };
    if active_state_settled(context_doc) {
        return Ok(false);
    }
    reservation_task_uuid(&first)?;
    Ok(!inputs.archive.live())
}

/// The shared A2A hold decision over targeted leader reads. The fence reads
/// the registry record first, then the context entry; the reservation and
/// archive records are fetched only where the fence would read them, so an
/// unresolved context or a context marker never spends a targeted read.
/// Registry, reservation and archive markers read as absent, the way the
/// fence's leader reads treat them. Shared by the session detail view and
/// the manual cleanup preview so both decide from the same facts; real
/// fences stay authoritative.
pub(crate) async fn leader_read_a2a_holds(
    store: &crate::nats_session_metadata::SessionMetadataStore,
    storage_key: &str,
) -> Result<A2aHoldSummary> {
    let registry =
        RecordFact::from_leader_value(store.leader_value(&registry_key(storage_key)).await?);
    let context = RecordFact::from_entry(
        store
            .leader_entry(&context_key(storage_key))
            .await?
            .as_ref(),
    );
    let reservation = match reservation_key_when_inspected(&context, &registry)? {
        Some(key) => RecordFact::from_leader_value(store.leader_value(&key).await?),
        None => RecordFact::Absent,
    };
    let archive = match archive_key_when_inspected(storage_key, &context, &registry, &reservation)?
    {
        Some(key) => RecordFact::from_leader_value(store.leader_value(&key).await?),
        None => RecordFact::Absent,
    };
    evaluate_a2a_holds(
        storage_key,
        &A2aHoldInputs {
            context,
            registry,
            reservation,
            archive,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fact(value: Value) -> RecordFact {
        RecordFact::Live(serde_json::to_vec(&value).unwrap().into())
    }

    /// Inputs with a live registry and a first-message reservation allocated
    /// to `storage`: the pair the unsettled-reservation rule inspects.
    fn storage_inputs(context: RecordFact, archive: RecordFact) -> A2aHoldInputs {
        A2aHoldInputs {
            context,
            registry: fact(registry_with_identity()),
            reservation: fact(reservation_for("storage")),
            archive,
        }
    }

    fn inputs(context: RecordFact, registry: RecordFact) -> A2aHoldInputs {
        A2aHoldInputs {
            context,
            registry,
            reservation: RecordFact::Absent,
            archive: RecordFact::Absent,
        }
    }

    fn active_state(state: Value) -> Value {
        json!({"state": {"active": state}})
    }

    fn registry_with_identity() -> Value {
        json!({"first": {"identity": {"user": "alice"}}})
    }

    fn reservation_for(storage: &str) -> Value {
        json!({"allocation": {"storage_key": storage, "task_id": "local.abc-123"}})
    }

    #[test]
    fn no_a2a_state_is_no_hold() {
        let summary = evaluate_a2a_holds("storage", &A2aHoldInputs::absent()).unwrap();
        assert_eq!(summary, A2aHoldSummary::default());
    }

    #[test]
    fn unresolved_active_context_is_a_hold() {
        let context = fact(active_state(json!({"stop_confirmed": false})));
        let summary = evaluate_a2a_holds("storage", &inputs(context, RecordFact::Absent)).unwrap();
        assert!(summary.unresolved_context);
        assert!(!summary.unsettled_first_reservation);
    }

    #[test]
    fn null_active_state_is_not_unresolved() {
        let context = fact(active_state(Value::Null));
        let summary = evaluate_a2a_holds("storage", &inputs(context, RecordFact::Absent)).unwrap();
        assert_eq!(summary, A2aHoldSummary::default());
    }

    #[test]
    fn retired_active_state_is_not_unresolved() {
        let context = fact(active_state(json!({
            "snapshot": {"task": {"status": {"state": "TASK_STATE_COMPLETED"}}},
            "stop_confirmed": true,
            "projections": {"archive": true, "message_mapping": true, "final_event": true},
            "publication": {"pending": null}
        })));
        let summary = evaluate_a2a_holds("storage", &inputs(context, RecordFact::Absent)).unwrap();
        assert!(!summary.unresolved_context);
    }

    #[test]
    fn context_marker_skips_first_reservation_check() {
        for marker in [Operation::Delete, Operation::Purge] {
            let registry = fact(registry_with_identity());
            let reservation = fact(reservation_for("storage"));
            let marked = A2aHoldInputs {
                context: RecordFact::Marker(marker),
                registry,
                reservation,
                archive: RecordFact::Absent,
            };
            let summary = evaluate_a2a_holds("storage", &marked).unwrap();
            assert!(!summary.unsettled_first_reservation);
            // A marker context also holds no unresolved state.
            assert!(!summary.unresolved_context);
        }
    }

    #[test]
    fn absent_context_with_unsettled_reservation_is_a_hold() {
        let registry = fact(registry_with_identity());
        let reservation = fact(reservation_for("storage"));
        let held = A2aHoldInputs {
            context: RecordFact::Absent,
            registry,
            reservation,
            archive: RecordFact::Absent,
        };
        assert!(
            evaluate_a2a_holds("storage", &held)
                .unwrap()
                .unsettled_first_reservation
        );

        let settled = A2aHoldInputs {
            archive: fact(json!({"kind": "archive"})),
            ..held
        };
        assert!(
            !evaluate_a2a_holds("storage", &settled)
                .unwrap()
                .unsettled_first_reservation
        );
    }

    #[test]
    fn null_active_context_with_unsettled_reservation_is_a_hold() {
        // A live Put context with a null active state still checks the
        // reservation, like the fence.
        let registry = fact(registry_with_identity());
        let reservation = fact(reservation_for("storage"));
        let held = A2aHoldInputs {
            context: fact(active_state(Value::Null)),
            registry,
            reservation,
            archive: RecordFact::Absent,
        };
        assert!(
            evaluate_a2a_holds("storage", &held)
                .unwrap()
                .unsettled_first_reservation
        );
    }

    #[test]
    fn non_null_active_state_satisfies_the_reservation_rule() {
        let settled = storage_inputs(
            fact(active_state(json!({"stop_confirmed": true}))),
            RecordFact::Absent,
        );
        // The context itself is unresolved here, but the reservation rule is
        // satisfied: the session is held for one reason, not two.
        let summary = evaluate_a2a_holds("storage", &settled).unwrap();
        assert!(summary.unresolved_context);
        assert!(!summary.unsettled_first_reservation);
    }

    #[test]
    fn reservation_for_another_session_is_no_hold() {
        let registry = fact(registry_with_identity());
        let reservation = fact(reservation_for("other-storage"));
        let inputs = A2aHoldInputs {
            context: RecordFact::Absent,
            registry,
            reservation,
            archive: RecordFact::Absent,
        };
        assert!(
            !evaluate_a2a_holds("storage", &inputs)
                .unwrap()
                .unsettled_first_reservation
        );
    }

    #[test]
    fn missing_or_marker_reservation_is_no_hold() {
        let registry = fact(registry_with_identity());
        for reservation in [RecordFact::Absent, RecordFact::Marker(Operation::Purge)] {
            let inputs = A2aHoldInputs {
                context: RecordFact::Absent,
                registry: registry.clone(),
                reservation,
                archive: RecordFact::Absent,
            };
            assert!(
                !evaluate_a2a_holds("storage", &inputs)
                    .unwrap()
                    .unsettled_first_reservation
            );
        }
    }

    #[test]
    fn marker_archive_counts_as_absent_and_holds() {
        let registry = fact(registry_with_identity());
        let reservation = fact(reservation_for("storage"));
        let inputs = A2aHoldInputs {
            context: RecordFact::Absent,
            registry,
            reservation,
            archive: RecordFact::Marker(Operation::Purge),
        };
        assert!(
            evaluate_a2a_holds("storage", &inputs)
                .unwrap()
                .unsettled_first_reservation
        );
    }

    /// The two holds are mutually exclusive, by the fence's own rule: a
    /// non-null active state is what makes a context unresolved, and the same
    /// non-null active state satisfies the first-reservation rule. The
    /// summary reports both flags, but the fence never refuses one session
    /// for both reasons at once.
    #[test]
    fn unresolved_context_settles_the_reservation_rule() {
        let inputs = storage_inputs(
            fact(active_state(json!({"stop_confirmed": false}))),
            RecordFact::Absent,
        );
        let summary = evaluate_a2a_holds("storage", &inputs).unwrap();
        assert!(summary.unresolved_context);
        assert!(!summary.unsettled_first_reservation);
        assert!(summary.any());
    }

    #[test]
    fn malformed_context_state_is_an_error_not_a_no_hold() {
        let context = RecordFact::Live(b"not json".as_ref().into());
        assert!(evaluate_a2a_holds("storage", &inputs(context, RecordFact::Absent)).is_err());

        let context = fact(json!({"unexpected": true}));
        assert!(evaluate_a2a_holds("storage", &inputs(context, RecordFact::Absent)).is_err());
    }

    #[test]
    fn malformed_registry_and_reservation_are_errors() {
        let registry = RecordFact::Live(b"not json".as_ref().into());
        assert!(evaluate_a2a_holds("storage", &inputs(RecordFact::Absent, registry)).is_err());

        let registry = fact(registry_with_identity());
        let reservation = RecordFact::Live(b"not json".as_ref().into());
        let inputs = A2aHoldInputs {
            context: RecordFact::Absent,
            registry,
            reservation,
            archive: RecordFact::Absent,
        };
        assert!(evaluate_a2a_holds("storage", &inputs).is_err());
    }

    #[test]
    fn invalid_reservation_task_fields_are_errors() {
        for task in [Value::Null, json!("no-dot")] {
            let registry = fact(registry_with_identity());
            let reservation =
                fact(json!({"allocation": {"storage_key": "storage", "task_id": task}}));
            let inputs = A2aHoldInputs {
                context: RecordFact::Absent,
                registry,
                reservation,
                archive: RecordFact::Absent,
            };
            assert!(evaluate_a2a_holds("storage", &inputs).is_err());
        }
    }

    #[test]
    fn key_derivation_helpers_match_fence_layout() {
        let storage = "abc123";
        assert_eq!(context_key(storage), "sessions/abc123/a2a/context");
        assert_eq!(
            registry_key(storage),
            format!("a2a.registry.{}", harnx_core::crypto::sha256(storage))
        );
        let registry = fact(registry_with_identity());
        let identity = registry_with_identity();
        let identity = identity.pointer("/first/identity").unwrap();
        assert_eq!(
            reservation_key_from_registry(&registry).unwrap().unwrap(),
            [
                "a2a/first-messages/",
                &harnx_core::crypto::sha256(&identity.to_string()),
            ]
            .concat()
        );
        let registry_fact = fact(registry_with_identity());
        let reservation = fact(reservation_for(storage));
        assert_eq!(
            archive_key_when_inspected(storage, &RecordFact::Absent, &registry_fact, &reservation)
                .unwrap()
                .unwrap(),
            format!("sessions/{storage}/a2a/archive/abc-123")
        );
        assert_eq!(
            archive_key_when_inspected("other", &RecordFact::Absent, &registry_fact, &reservation)
                .unwrap(),
            None
        );
    }

    #[test]
    fn archive_key_derivation_follows_the_fence_order() {
        let registry = fact(registry_with_identity());
        // A marker context skips the first-reservation check entirely: a
        // reservation payload the fence would never read cannot fail it.
        let marker = RecordFact::Marker(Operation::Purge);
        let malformed = RecordFact::Live(b"not json".to_vec().into());
        assert_eq!(
            archive_key_when_inspected("storage", &marker, &registry, &malformed).unwrap(),
            None
        );
        // A non-null active state settles the rule before the task identity
        // is validated, so a malformed task id is never inspected.
        let settled = fact(active_state(json!({"stop_confirmed": true})));
        let bad_task = fact(json!({"allocation": {"storage_key": "storage", "task_id": "nodot"}}));
        assert_eq!(
            archive_key_when_inspected("storage", &settled, &registry, &bad_task).unwrap(),
            None
        );
        // Where the fence does inspect the task identity, malformed state is
        // an error, never a silent no-hold.
        assert!(
            archive_key_when_inspected("storage", &RecordFact::Absent, &registry, &bad_task)
                .is_err()
        );
        // The inspectable path derives the archive key.
        let reservation = fact(reservation_for("storage"));
        assert_eq!(
            archive_key_when_inspected("storage", &RecordFact::Absent, &registry, &reservation)
                .unwrap(),
            Some("sessions/storage/a2a/archive/abc-123".to_string())
        );
    }

    #[test]
    fn marker_context_skips_a_malformed_reservation() {
        let inputs = A2aHoldInputs {
            context: RecordFact::Marker(Operation::Delete),
            registry: fact(registry_with_identity()),
            reservation: RecordFact::Live(b"not json".to_vec().into()),
            archive: RecordFact::Absent,
        };
        let summary = evaluate_a2a_holds("storage", &inputs).unwrap();
        assert!(
            !summary.any(),
            "marker context: no hold, and the reservation is never parsed"
        );
    }

    /// The fence's short-circuit order, with a malformed reservation paired
    /// against every context state it treats differently. Shared fixtures:
    /// every case's facts, and the three entry points that must agree with
    /// the fence — the hold decision, the archive-key derivation and the
    /// targeted reader's reservation-key decision.
    fn fence_matrix_inputs(context: RecordFact, reservation: RecordFact) -> A2aHoldInputs {
        A2aHoldInputs {
            context,
            registry: fact(registry_with_identity()),
            reservation,
            archive: RecordFact::Absent,
        }
    }

    fn malformed_fact() -> RecordFact {
        RecordFact::Live(b"not json".to_vec().into())
    }

    fn retired_fact() -> RecordFact {
        fact(active_state(json!({
            "snapshot": {"task": {"status": {"state": "TASK_STATE_COMPLETED"}}},
            "stop_confirmed": true,
            "projections": {"archive": true, "message_mapping": true, "final_event": true},
            "publication": {"pending": null}
        })))
    }

    /// An unresolved context holds the session; the fence refuses it before
    /// it ever reads the first-message reservation, so no entry point may
    /// parse the reservation or derive a key to fetch it.
    #[test]
    fn unresolved_context_short_circuits_before_the_reservation() {
        let unresolved = fact(active_state(json!({"stop_confirmed": false})));
        let registry = fact(registry_with_identity());
        let inputs = fence_matrix_inputs(unresolved.clone(), malformed_fact());
        let summary = evaluate_a2a_holds("storage", &inputs).unwrap();
        assert!(summary.unresolved_context);
        assert!(!summary.unsettled_first_reservation);
        assert_eq!(
            archive_key_when_inspected("storage", &unresolved, &registry, &malformed_fact())
                .unwrap(),
            None
        );
        assert_eq!(
            reservation_key_when_inspected(&unresolved, &registry).unwrap(),
            None,
            "no targeted reservation read for an unresolved context"
        );
    }

    /// A malformed context errors before its reservation is parsed, and the
    /// registry record parses before both — the fence's own error order.
    #[test]
    fn malformed_context_errors_before_its_reservation() {
        let registry = fact(registry_with_identity());
        let inputs = fence_matrix_inputs(malformed_fact(), malformed_fact());
        let error = evaluate_a2a_holds("storage", &inputs).unwrap_err();
        assert!(
            error.to_string().contains("context"),
            "context error first, got: {error:#}"
        );
        let error =
            archive_key_when_inspected("storage", &malformed_fact(), &registry, &malformed_fact())
                .unwrap_err();
        assert!(
            error.to_string().contains("context"),
            "context error first, got: {error:#}"
        );

        let malformed_registry_inputs = A2aHoldInputs {
            registry: malformed_fact(),
            ..inputs
        };
        let error = evaluate_a2a_holds("storage", &malformed_registry_inputs).unwrap_err();
        assert!(
            error.to_string().contains("registry"),
            "registry error before the context error, got: {error:#}"
        );
        let error = archive_key_when_inspected(
            "storage",
            &malformed_fact(),
            &malformed_fact(),
            &malformed_fact(),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("registry"),
            "registry error before the context error, got: {error:#}"
        );
    }

    /// A retired context still parses the reservation JSON — exactly where
    /// the fence's `ensure_first_settled` does — but never its task identity.
    #[test]
    fn retired_context_still_parses_the_reservation_not_its_task() {
        let retired = retired_fact();
        let registry = fact(registry_with_identity());
        let inputs = fence_matrix_inputs(retired.clone(), malformed_fact());
        let error = evaluate_a2a_holds("storage", &inputs).unwrap_err();
        assert!(
            error.to_string().contains("reservation"),
            "retired context still parses the reservation, got: {error:#}"
        );
        let error = archive_key_when_inspected("storage", &retired, &registry, &malformed_fact())
            .unwrap_err();
        assert!(
            error.to_string().contains("reservation"),
            "retired context still parses the reservation, got: {error:#}"
        );
        let bad_task = fact(json!({"allocation": {"storage_key": "storage", "task_id": "nodot"}}));
        let inputs = fence_matrix_inputs(retired.clone(), bad_task.clone());
        assert!(
            !evaluate_a2a_holds("storage", &inputs).unwrap().any(),
            "the settled active state satisfies the rule without the task identity"
        );
        assert_eq!(
            archive_key_when_inspected("storage", &retired, &registry, &bad_task).unwrap(),
            None
        );
    }

    /// A marker context skips the reservation entirely — no hold, no archive
    /// key, no targeted read — while contexts the fence does check still
    /// derive the reservation key.
    #[test]
    fn marker_context_never_derives_a_reservation_key() {
        let registry = fact(registry_with_identity());
        let marker = RecordFact::Marker(Operation::Delete);
        assert!(!evaluate_a2a_holds(
            "storage",
            &fence_matrix_inputs(marker.clone(), malformed_fact())
        )
        .unwrap()
        .any());
        assert_eq!(
            archive_key_when_inspected("storage", &marker, &registry, &malformed_fact()).unwrap(),
            None
        );
        assert_eq!(
            reservation_key_when_inspected(&marker, &registry).unwrap(),
            None
        );

        let expected = [
            "a2a/first-messages/",
            &harnx_core::crypto::sha256(r#"{"user":"alice"}"#),
        ]
        .concat();
        assert_eq!(
            reservation_key_when_inspected(&retired_fact(), &registry).unwrap(),
            Some(expected.clone()),
            "a retired context still reads the reservation"
        );
        assert_eq!(
            reservation_key_when_inspected(&RecordFact::Absent, &registry).unwrap(),
            Some(expected),
            "an absent context still reads the reservation"
        );
    }
}

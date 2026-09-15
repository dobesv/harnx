use super::*;
use crate::{OperationKind, OperationRef};

fn operation() -> Operation {
    Operation::preparing(
        OperationRef::new("state", "generation"),
        OperationKind::Session,
        None,
    )
}

fn decision(id: &str) -> StopDecision {
    StopDecision {
        cancellation_id: id.into(),
        accepted_at: Utc::now(),
        reason: "user interrupt".into(),
    }
}

fn assert_interrupted_rejects_work(op: &mut Operation) {
    assert_eq!(op.logical_state(), LogicalState::Interrupted);
    assert!(!op.accepts_work());
    assert!(op.transition(OperationState::Running).is_err());
    assert!(op.transition(OperationState::Completed).is_err());
}

fn assert_cancelled_with_stop(op: &Operation, stop: StopDecision) {
    assert_eq!(op.state, OperationState::Cancelled);
    assert_eq!(op.logical_state(), LogicalState::Interrupted);
    assert_eq!(op.stop_decision, Some(stop));
    assert!(!op.allows_continuation());
}

fn assert_unfinished_cleanup(op: &Operation, expected: CleanupState) {
    assert_eq!(op.cleanup_state(), expected);
    assert!(!op.cleanup_confirmed());
}

fn assert_no_accepted_stop(op: &Operation) {
    assert!(op.stop_decision.is_none());
    assert!(!op.is_stopped());
    assert!(!op.can_replace_generation());
}

#[test]
fn interruption_never_reopens_through_cleanup_retry_completion_or_serialization() -> Result<()> {
    for initial in [OperationState::Preparing, OperationState::Running] {
        let mut op = operation();
        op.state = initial;
        let stop = decision("first");
        op.accept_interrupt(stop.clone())?;
        assert_interrupted_rejects_work(&mut op);
        op.transition(OperationState::Unconfirmed)?;
        op.request_cancel("retry", true)?;
        op.transition(OperationState::Quiescing)?;
        op.owner_stopped = true;
        op.cancel_recorded = true;
        op.reconcile_completion()?;
        op.accept_interrupt(decision("duplicate"))?;
        let op: Operation = serde_json::from_slice(&serde_json::to_vec(&op)?)?;
        assert_cancelled_with_stop(&op, stop);
    }
    Ok(())
}

#[test]
fn only_logically_completed_or_interrupted_generations_can_be_replaced() -> Result<()> {
    for state in [LogicalState::Preparing, LogicalState::Running] {
        assert!(!state.can_replace_generation());
        assert!(!state.is_stopped());
    }
    for state in [LogicalState::Completed, LogicalState::Interrupted] {
        assert!(state.can_replace_generation());
        assert!(state.is_stopped());
        assert!(!state.accepts_work());
    }
    let mut op = operation();
    op.transition(OperationState::Running)?;
    assert!(!op.can_replace_generation());
    op.owner_stopped = true;
    op.reconcile_completion()?;
    assert_eq!(op.logical_state(), LogicalState::Completed);
    assert!(op.can_replace_generation());
    assert!(op.accept_interrupt(decision("too-late")).is_err());
    Ok(())
}

#[test]
fn interrupted_is_not_cleanup_terminal_or_prunable() -> Result<()> {
    let mut op = operation();
    op.children.insert(OperationRef::new("state", "child"));
    op.accept_interrupt(decision("stop"))?;
    assert!(op.is_stopped());
    assert!(op.can_replace_generation());
    assert_unfinished_cleanup(&op, CleanupState::Pending);
    assert!(!op.can_prune());
    assert!(!op.state.is_lifecycle_terminal());
    op.owner_stopped = true;
    op.transition(OperationState::Unconfirmed)?;
    assert_unfinished_cleanup(&op, CleanupState::Unconfirmed);
    assert!(!op.can_prune());
    assert_eq!(op.logical_state(), LogicalState::Interrupted);
    Ok(())
}

#[test]
fn transcript_projection_neither_accepts_interruption_nor_confirms_cleanup() -> Result<()> {
    let mut op = operation();
    op.request_cancel("legacy", false)?;
    op.cancel_recorded = true;
    assert_no_accepted_stop(&op);
    assert_eq!(op.cleanup_state(), CleanupState::Pending);
    op.cancel_recorded = false;
    op.accept_interrupt(decision("accepted"))?;
    assert!(!op.cancel_recorded);
    op.owner_stopped = true;
    op.reconcile_completion()?;
    assert!(op.cleanup_confirmed());
    assert!(!op.can_prune(), "legacy projection still pending");
    op.cancel_recorded = true;
    op.reconcile_completion()?;
    assert!(op.can_prune());
    assert_eq!(op.logical_state(), LogicalState::Interrupted);
    Ok(())
}

#[test]
fn legacy_documents_derive_dimensions_without_fabricating_stop_acceptance() -> Result<()> {
    for state in [
        OperationState::Preparing,
        OperationState::Running,
        OperationState::Cancelled,
    ] {
        let mut op = operation();
        op.state = state;
        let mut document = serde_json::to_value(&op)?;
        document.as_object_mut().unwrap().remove("stop_decision");
        let decoded: Operation = serde_json::from_value(document)?;
        assert!(decoded.stop_decision.is_none());
        assert_eq!(decoded.logical_state(), op.logical_state());
        assert_eq!(
            decoded.can_replace_generation(),
            state.is_lifecycle_terminal()
        );
    }
    Ok(())
}

#[test]
fn abandonment_does_not_confirm_physical_cleanup() -> Result<()> {
    let mut op = operation();
    op.accept_interrupt(decision("stop"))?;
    op.transition(OperationState::Unconfirmed)?;
    op.abandon_unconfirmed()?;
    assert!(op.can_prune(), "preserve legacy abandonment retirement");
    assert!(op.is_stopped());
    assert_unfinished_cleanup(&op, CleanupState::Unconfirmed);
    Ok(())
}

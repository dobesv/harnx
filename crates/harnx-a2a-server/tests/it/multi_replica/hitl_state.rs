//! Shared parked-state predicate for independent replica fixtures.
use a2a_lf::TaskState;
use harnx_a2a_server::store::context::ContextSnapshot;

pub(super) fn is_parked_waiting(context: &ContextSnapshot, request_after: u64) -> bool {
    let Some(active) = context.document.state.active.as_ref() else {
        return false;
    };
    if context.document.owner.is_some() || active.publication.pending.is_some() {
        return false;
    }
    if active.snapshot.task.status.state != TaskState::InputRequired {
        return false;
    }
    active
        .hitl_checkpoint
        .as_ref()
        .is_some_and(|c| c.request.request_seq > request_after)
}

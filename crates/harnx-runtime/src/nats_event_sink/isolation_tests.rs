//! `LiveEventState`'s sequence fence: what a `Cancel` drops, and what a fork
//! keeps of it.
use super::*;
use harnx_core::event::TurnEvent;

#[test]
fn frontend_drops_live_events_below_cancel_seq() {
    let state = LiveEventState::default();
    let env = |after_seq| AdvisoryEnvelope::new(after_seq, AgentEvent::Turn(TurnEvent::Started));
    assert!(state.should_render(&env(41), 40));
    state.accept_interrupt(42);
    assert!(!state.should_render(&env(41), 40));
    // The fence drops what the `Cancel` overtook, not the interruption's own
    // output: an advisory stamped at the `Cancel` still renders.
    assert!(state.should_render(&env(42), 40));
    assert!(state.should_render(&env(43), 40));
}

#[test]
fn fork_keeps_the_cancel_fence_but_is_a_new_attachment() {
    let state = LiveEventState::default();
    state.accept_interrupt(10);
    let forked = state.fork();
    assert!(!forked.same_attachment(&state));
    assert!(!forked.should_render(
        &AdvisoryEnvelope::new(9, AgentEvent::Turn(TurnEvent::Started)),
        0
    ));
}

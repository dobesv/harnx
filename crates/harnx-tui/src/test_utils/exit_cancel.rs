//! Driving a pending exit cancel the way the event loop does.

use crate::types::Tui;
use std::time::Duration;

/// Observe a pending exit cancel the way the event loop does: one poll per
/// tick, with the runtime free to run other tasks in between. True once the
/// cancel has settled, false if it is still pending after `max_ticks`.
pub(crate) async fn settle_exit_cancel(tui: &mut Tui, max_ticks: usize) -> bool {
    for _ in 0..max_ticks {
        tui.poll_pending_exit_cancel().await;
        if tui.pending_exit_cancel.is_none() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    false
}

/// Wait until the spawned exit cancel task has finished without observing
/// its outcome, so a test can time the poll that processes it.
pub(crate) async fn wait_exit_cancel_task_finished(tui: &Tui) {
    while tui
        .pending_exit_cancel
        .as_ref()
        .is_some_and(|task| !task.is_finished())
    {
        tokio::task::yield_now().await;
    }
}

//! Correlate orphan-watchdog failures with the invocation that was monitored.

use crate::types::{
    MonitoredSessionKey, SubAgentInvocationProgress, SubAgentStatus, TranscriptItem, Tui,
};
use harnx_core::event::SubAgentProgressStatus;

impl Tui {
    pub(super) fn fail_monitored_invocation(
        &mut self,
        key: &MonitoredSessionKey,
        invocation_id: &str,
    ) {
        for items in std::iter::once(&mut self.app.transcript)
            .chain(
                self.app
                    .monitored_sessions
                    .values_mut()
                    .map(|state| &mut state.transcript),
            )
            .flatten()
        {
            fail_row(items, key, invocation_id);
        }
        for view in &mut self.app.subagent_view_stack {
            if &view.key == key
                && view
                    .progress
                    .as_mut()
                    .is_some_and(|progress| fail_progress(progress, invocation_id))
            {
                view.status = SubAgentStatus::Failed;
            }
        }
    }
}

fn fail_progress(progress: &mut SubAgentInvocationProgress, invocation_id: &str) -> bool {
    if progress.snapshot.invocation_id != invocation_id
        || progress.snapshot.status != SubAgentProgressStatus::Running
    {
        return false;
    }
    progress.snapshot.elapsed_ms = progress.elapsed_ms();
    progress.snapshot.status = SubAgentProgressStatus::Failed;
    true
}

fn fail_row(item: &mut TranscriptItem, key: &MonitoredSessionKey, invocation_id: &str) {
    let TranscriptItem::SubAgentSession {
        key: row_key,
        status,
        progress: Some(progress),
        ..
    } = item
    else {
        return;
    };
    if row_key == key && fail_progress(progress, invocation_id) {
        *status = SubAgentStatus::Failed;
    }
}

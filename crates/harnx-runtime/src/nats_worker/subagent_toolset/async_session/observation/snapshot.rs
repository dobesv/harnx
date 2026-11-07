//! Read-only lifecycle scan. Keep one bounded tool round, not a session replica.
use super::{
    budget::{Budget, Stop},
    scope::Scope,
    SessionPromptStatus,
};
use crate::nats_session_log::{LogWindow, NatsSessionLog, SCAN_BYTE_LIMIT, SCAN_RECORD_LIMIT};
use crate::nats_session_metadata::InvocationAdmission;
use harnx_core::session::SessionLogEntry;

#[derive(Default)]
pub(super) struct Snapshot {
    pub(super) prompt: Option<u64>,
    pub(super) through: u64,
    pub(super) terminal: Option<(u64, SessionPromptStatus)>,
    pub(super) complete: bool,
    gap: bool,
    unsafe_live: bool,
    identity_mismatch: bool,
    round: Vec<(u64, SessionLogEntry)>,
    round_bytes: usize,
}

impl Snapshot {
    pub(super) async fn refresh(
        &mut self,
        log: &NatsSessionLog,
        scope: &Scope,
        budget: &Budget,
    ) -> Result<(), Stop> {
        if self.terminal.is_some() {
            return Ok(());
        }
        self.complete = false;
        let tail = budget
            .io(log.last_entry_async())
            .await?
            .map_err(|_| Stop::Unavailable)?
            .map_or(0, |(seq, _)| seq);
        while self.through < tail {
            let window = budget
                .io(log.load_window_async(Some(self.through + 1), Some(tail)))
                .await?
                .map_err(|_| Stop::Unavailable)?;
            self.consume(window, scope, budget)?;
            if self.terminal.is_some() {
                break;
            }
            // Ready broker responses must not starve cancellation or deadlines.
            tokio::task::yield_now().await;
        }
        self.complete = true;
        Ok(())
    }

    fn consume(&mut self, window: LogWindow, scope: &Scope, budget: &Budget) -> Result<(), Stop> {
        let previous = self.through;
        for (seq, entry) in window.entries {
            budget.check()?;
            self.advance(seq, seq != self.through + 1);
            self.accept(seq, entry, scope);
            if self.terminal.is_some() {
                return Ok(());
            }
        }
        self.accept_scan_progress(window.scanned_through);
        if self.through == previous {
            return Err(Stop::Unavailable);
        }
        Ok(())
    }

    fn accept_scan_progress(&mut self, scanned: Option<u64>) {
        if let Some(scanned) = scanned.filter(|seq| *seq > self.through) {
            self.advance(scanned, true);
        }
    }

    fn advance(&mut self, seq: u64, gap: bool) {
        // Even a pre-prompt gap could hide the original before a compaction copy.
        // Without durable sequence identity, choosing that copy is not safe.
        self.gap |= gap;
        self.through = seq;
    }

    fn accept(&mut self, seq: u64, entry: SessionLogEntry, scope: &Scope) {
        if self.prompt.is_none() {
            self.find_prompt(seq, &entry, scope);
            return;
        }
        let prompt = self.prompt.expect("prompt checked");
        // Only user prompts reach this scanner, never CompactRequest. The shared
        // helper's one-row form therefore preserves its terminal coverage rules.
        let row = (seq, entry);
        if crate::nats_session::invocation_terminal_seq(std::slice::from_ref(&row), prompt)
            .is_some()
        {
            self.terminal = (!self.gap && !self.identity_mismatch)
                .then(|| (seq, terminal_status(&row.1, &scope.admission)));
            return;
        }
        self.track_round(seq, row.1);
    }

    fn find_prompt(&mut self, seq: u64, entry: &SessionLogEntry, scope: &Scope) {
        if let SessionLogEntry::Message {
            id: Some(id),
            role,
            content,
            ..
        } = entry
        {
            if role.is_user() && id == &scope.prompt_id {
                self.identity_mismatch |= Some(content) != scope.admission.prompt_content.as_ref();
                if !self.identity_mismatch {
                    self.prompt = Some(seq);
                }
            }
        }
    }

    fn track_round(&mut self, seq: u64, entry: SessionLogEntry) {
        match &entry {
            SessionLogEntry::Clear
            | SessionLogEntry::Compress { .. }
            | SessionLogEntry::EditEntries { .. }
            | SessionLogEntry::Rewind { .. } => {
                // Raw terminals survive mutations; live approval derivation needs
                // evidence that these bounded windows cannot reconstruct.
                self.unsafe_live = true;
                self.round.clear();
            }
            SessionLogEntry::ToolCalls { .. } => {
                self.round.clear();
                self.round_bytes = 0;
                self.keep_round_entry(seq, entry);
            }
            SessionLogEntry::HitlApprovalRequested { .. }
            | SessionLogEntry::HitlApprovalDecision { .. }
            | SessionLogEntry::ToolResults { .. } => {
                if self.round.is_empty() {
                    self.unsafe_live = true;
                }
                self.keep_round_entry(seq, entry);
            }
            _ => {}
        }
    }

    fn keep_round_entry(&mut self, seq: u64, entry: SessionLogEntry) {
        self.round_bytes += serde_json::to_vec(&entry).map_or(SCAN_BYTE_LIMIT, |v| v.len());
        if self.round_bytes > SCAN_BYTE_LIMIT || self.round.len() >= SCAN_RECORD_LIMIT {
            self.unsafe_live = true;
            self.round.clear();
        } else {
            self.round.push((seq, entry));
        }
    }

    pub(super) fn evidence_error(&self) -> Option<&'static str> {
        if self.identity_mismatch {
            Some("prompt_content_mismatch")
        } else if self.gap {
            Some("incomplete_durable_history")
        } else if self.prompt.is_none() {
            Some("original_prompt_not_found")
        } else {
            None
        }
    }

    pub(super) fn status(&self) -> SessionPromptStatus {
        if let Some((_, status)) = self.terminal {
            return status;
        }
        let live_is_proven = self.complete && !self.unsafe_live;
        if !live_is_proven || self.evidence_error().is_some() {
            return SessionPromptStatus::Unknown;
        }
        match crate::nats_worker::derive_pending_hitl_approvals(&self.round) {
            Ok(approvals) if !approvals.is_empty() => SessionPromptStatus::Blocked,
            Ok(_) => SessionPromptStatus::Pending,
            Err(_) => SessionPromptStatus::Unknown,
        }
    }

    pub(super) fn output_end(&self) -> u64 {
        self.terminal.map_or(self.through, |(seq, _)| seq)
    }
}

fn terminal_status(
    entry: &SessionLogEntry,
    admission: &InvocationAdmission,
) -> SessionPromptStatus {
    match entry {
        SessionLogEntry::TurnEnd { .. } => SessionPromptStatus::Completed,
        SessionLogEntry::Error { .. } => SessionPromptStatus::Failed,
        SessionLogEntry::Cancel { requested_by, .. } => {
            let timeout = requested_by
                .as_deref()
                .and_then(crate::parse_timeout_terminal);
            if timeout.is_some_and(|t| {
                t.invocation_id == admission.invocation_id.as_str()
                    && t.run_id == admission.run_id.as_str()
            }) {
                SessionPromptStatus::TimedOut
            } else {
                SessionPromptStatus::Cancelled
            }
        }
        _ => SessionPromptStatus::Unknown,
    }
}

use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NatsMetricsSnapshot {
    pub active_sessions_per_worker: u64,
    pub lease_acquisitions: u64,
    pub lease_losses: u64,
    pub fenced_writes_rejected: u64,
    pub resumes: u64,
    pub interrupt_errors_synthesized: u64,
}

static ACTIVE_SESSIONS_PER_WORKER: AtomicU64 = AtomicU64::new(0);
static LEASE_ACQUISITIONS: AtomicU64 = AtomicU64::new(0);
static LEASE_LOSSES: AtomicU64 = AtomicU64::new(0);
static FENCED_WRITES_REJECTED: AtomicU64 = AtomicU64::new(0);
static RESUMES: AtomicU64 = AtomicU64::new(0);
static INTERRUPT_ERRORS_SYNTHESIZED: AtomicU64 = AtomicU64::new(0);

pub fn active_session_started() {
    ACTIVE_SESSIONS_PER_WORKER.fetch_add(1, Ordering::SeqCst);
}

pub fn active_session_finished() {
    ACTIVE_SESSIONS_PER_WORKER
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
            Some(value.saturating_sub(1))
        })
        .ok();
}

pub fn lease_acquired() {
    LEASE_ACQUISITIONS.fetch_add(1, Ordering::SeqCst);
}

pub fn lease_lost() {
    LEASE_LOSSES.fetch_add(1, Ordering::SeqCst);
}

pub fn fenced_write_rejected() {
    FENCED_WRITES_REJECTED.fetch_add(1, Ordering::SeqCst);
}

pub fn resume_detected() {
    RESUMES.fetch_add(1, Ordering::SeqCst);
}

pub fn interrupt_error_synthesized() {
    INTERRUPT_ERRORS_SYNTHESIZED.fetch_add(1, Ordering::SeqCst);
}

pub fn snapshot() -> NatsMetricsSnapshot {
    NatsMetricsSnapshot {
        active_sessions_per_worker: ACTIVE_SESSIONS_PER_WORKER.load(Ordering::SeqCst),
        lease_acquisitions: LEASE_ACQUISITIONS.load(Ordering::SeqCst),
        lease_losses: LEASE_LOSSES.load(Ordering::SeqCst),
        fenced_writes_rejected: FENCED_WRITES_REJECTED.load(Ordering::SeqCst),
        resumes: RESUMES.load(Ordering::SeqCst),
        interrupt_errors_synthesized: INTERRUPT_ERRORS_SYNTHESIZED.load(Ordering::SeqCst),
    }
}

#[cfg(test)]
pub fn reset_for_test() {
    ACTIVE_SESSIONS_PER_WORKER.store(0, Ordering::SeqCst);
    LEASE_ACQUISITIONS.store(0, Ordering::SeqCst);
    LEASE_LOSSES.store(0, Ordering::SeqCst);
    FENCED_WRITES_REJECTED.store(0, Ordering::SeqCst);
    RESUMES.store(0, Ordering::SeqCst);
    INTERRUPT_ERRORS_SYNTHESIZED.store(0, Ordering::SeqCst);
}

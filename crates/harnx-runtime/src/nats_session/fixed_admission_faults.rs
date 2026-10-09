//! Deterministic fixed-append boundary controls. Non-default test feature only.
use super::NatsSession;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::sync::Semaphore;

pub struct FixedAdmissionFaults {
    pause: AtomicBool,
    lost_ack: AtomicBool,
    reached: Semaphore,
    resume: Semaphore,
}

impl FixedAdmissionFaults {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            pause: AtomicBool::new(false),
            lost_ack: AtomicBool::new(false),
            reached: Semaphore::new(0),
            resume: Semaphore::new(0),
        })
    }

    pub fn pause_next(self: &Arc<Self>) -> FixedAppendPause {
        self.pause.store(true, Ordering::SeqCst);
        FixedAppendPause {
            faults: self.clone(),
        }
    }

    pub fn lose_next_ack(&self) {
        self.lost_ack.store(true, Ordering::SeqCst);
    }
    pub(crate) fn drop_ack(&self) -> bool {
        self.lost_ack.swap(false, Ordering::SeqCst)
    }

    pub(crate) async fn before_append(&self) {
        if self.pause.swap(false, Ordering::SeqCst) {
            self.reached.add_permits(1);
            self.resume.acquire().await.unwrap().forget();
        }
    }
}

pub struct FixedAppendPause {
    faults: Arc<FixedAdmissionFaults>,
}
impl FixedAppendPause {
    pub async fn wait_reached(&self) -> anyhow::Result<()> {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.faults.reached.acquire(),
        )
        .await??
        .forget();
        Ok(())
    }
}
impl Drop for FixedAppendPause {
    fn drop(&mut self) {
        self.faults.resume.add_permits(1);
    }
}
impl NatsSession {
    pub fn with_fixed_admission_faults(mut self, faults: Arc<FixedAdmissionFaults>) -> Self {
        self.fixed_admission_faults = Some(faults);
        self
    }
}

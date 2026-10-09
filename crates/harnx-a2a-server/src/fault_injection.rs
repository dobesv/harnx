//! Opt-in test controls. Never enabled by default in server builds.
//! Each runner owns its controls; no process-global state or timing guesses.
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::{watch, Semaphore};

#[derive(Clone, Copy, Debug)]
pub enum Boundary {
    DedupeMiss,
    Claim,
    BeforeAdmission,
    AfterAdmission,
    Publication,
    OrphanCancel,
    ContextCas,
    FirstReservation,
    SessionInitialized,
    MessageMapping,
    Activation,
    InitializationWait,
    OutboxCommitted,
    EventPublished,
    OutboxCleared,
    WatermarkCaptured,
    SnapshotCaptured,
    ReaderCreated,
    TerminalOutboxCommitted,
}
impl Boundary {
    fn index(self) -> usize {
        self as usize
    }
}

struct Point {
    count: AtomicUsize,
    reached: watch::Sender<usize>,
    pause: parking_lot::Mutex<Option<(usize, Arc<Semaphore>)>>,
}
impl Default for Point {
    fn default() -> Self {
        Self {
            count: AtomicUsize::new(0),
            reached: watch::channel(0).0,
            pause: Default::default(),
        }
    }
}

#[derive(Default)]
pub struct FaultHooks {
    points: [Point; 19],
    context_ack_loss: AtomicBool,
    first_ack_loss: AtomicBool,
    terminal_ack_loss: AtomicBool,
    event_ack_loss: AtomicBool,
}
impl FaultHooks {
    pub fn lose_next_event_ack(&self) {
        self.event_ack_loss.store(true, Ordering::SeqCst);
    }
    pub(crate) fn take_event_ack_loss(&self) -> bool {
        self.event_ack_loss.swap(false, Ordering::SeqCst)
    }
    pub fn lose_next_terminal_ack(&self) {
        self.terminal_ack_loss.store(true, Ordering::SeqCst);
    }
    pub(crate) fn take_terminal_ack_loss(&self) -> bool {
        self.terminal_ack_loss.swap(false, Ordering::SeqCst)
    }
    pub fn lose_next_first_ack(&self) {
        self.first_ack_loss.store(true, Ordering::SeqCst);
    }
    pub(crate) fn take_first_ack_loss(&self) -> bool {
        self.first_ack_loss.swap(false, Ordering::SeqCst)
    }
    pub fn lose_next_context_ack(&self) {
        self.context_ack_loss.store(true, Ordering::SeqCst);
    }
    pub(crate) fn take_context_ack_loss(&self) -> bool {
        self.context_ack_loss.swap(false, Ordering::SeqCst)
    }

    pub fn count(&self, boundary: Boundary) -> usize {
        self.points[boundary.index()].count.load(Ordering::SeqCst)
    }

    /// Pause exactly one future visit. Dropping the guard releases blocked work,
    /// including when a test assertion panics.
    pub fn pause(&self, boundary: Boundary, visit: usize) -> Pause {
        let point = &self.points[boundary.index()];
        let mut armed = point.pause.lock();
        assert!(armed.is_none(), "boundary already armed");
        assert!(
            visit > point.count.load(Ordering::SeqCst),
            "visit already reached"
        );
        let gate = Arc::new(Semaphore::new(0));
        *armed = Some((visit, gate.clone()));
        Pause {
            reached: point.reached.subscribe(),
            visit,
            gate,
        }
    }

    pub async fn checkpoint(&self, boundary: Boundary) {
        let point = &self.points[boundary.index()];
        let visit = point.count.fetch_add(1, Ordering::SeqCst) + 1;
        let gate = {
            let mut armed = point.pause.lock();
            if armed.as_ref().is_some_and(|(target, _)| *target == visit) {
                armed.take().map(|(_, gate)| gate)
            } else {
                None
            }
        };
        point
            .reached
            .send_modify(|count| *count = (*count).max(visit));
        if let Some(gate) = gate {
            gate.acquire().await.expect("pause gate closed").forget();
        }
    }
}

pub struct Pause {
    reached: watch::Receiver<usize>,
    visit: usize,
    gate: Arc<Semaphore>,
}
impl Pause {
    pub async fn reached(&mut self) {
        self.reached
            .wait_for(|count| *count >= self.visit)
            .await
            .expect("hooks dropped");
    }
}
impl Drop for Pause {
    fn drop(&mut self) {
        self.gate.add_permits(1);
    }
}

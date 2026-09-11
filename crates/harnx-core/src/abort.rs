//! Event-driven Ctrl-C / Ctrl-D cancellation shared by long-running operations.

use std::sync::Arc;
use tokio::sync::watch;

pub type AbortSignal = Arc<AbortSignalInner>;

pub struct AbortSignalInner {
    state: watch::Sender<u8>,
}

pub fn create_abort_signal() -> AbortSignal {
    AbortSignalInner::new()
}

impl AbortSignalInner {
    pub fn new() -> AbortSignal {
        let (state, _) = watch::channel(0);
        Arc::new(Self { state })
    }

    pub fn aborted(&self) -> bool {
        *self.state.borrow() != 0
    }

    pub fn aborted_ctrlc(&self) -> bool {
        *self.state.borrow() & 1 != 0
    }

    pub fn aborted_ctrld(&self) -> bool {
        *self.state.borrow() & 2 != 0
    }

    pub fn reset(&self) {
        self.state.send_replace(0);
    }

    pub fn set_ctrlc(&self) {
        self.state.send_modify(|state| *state |= 1);
    }

    pub fn set_ctrld(&self) {
        self.state.send_modify(|state| *state |= 2);
    }
}

pub async fn wait_abort_signal(abort_signal: &AbortSignal) {
    let mut state = abort_signal.state.subscribe();
    let _ = state.wait_for(|state| *state != 0).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::FutureExt;

    #[tokio::test]
    async fn abort_is_ready_on_the_next_poll_for_every_waiter() {
        let signal = create_abort_signal();
        let first = wait_abort_signal(&signal);
        let second = wait_abort_signal(&signal);
        tokio::pin!(first, second);
        assert!(first.as_mut().now_or_never().is_none());
        assert!(second.as_mut().now_or_never().is_none());
        signal.set_ctrlc();
        assert!(first.as_mut().now_or_never().is_some());
        assert!(second.as_mut().now_or_never().is_some());
        assert!(wait_abort_signal(&signal).now_or_never().is_some());
        assert!(signal.aborted_ctrlc());
        assert!(!signal.aborted_ctrld());
    }

    #[test]
    fn reset_and_both_interrupt_kinds_preserve_the_public_api() {
        let signal = create_abort_signal();
        signal.set_ctrlc();
        signal.set_ctrld();
        assert!(signal.aborted_ctrlc() && signal.aborted_ctrld());
        signal.reset();
        assert!(!signal.aborted());
        signal.set_ctrld();
        assert!(signal.aborted() && signal.aborted_ctrld());
        assert!(!signal.aborted_ctrlc());
    }
}

//! An unfinished physical dispatch is never a reason to retry implicitly.

use parking_lot::Mutex;
use std::sync::Arc;

#[derive(Default)]
pub(super) struct Effects(Mutex<EffectState>);

#[derive(Default)]
struct EffectState {
    pending: usize,
    unknown: bool,
}

impl Effects {
    pub fn outcome_unknown(&self) -> bool {
        let state = self.0.lock();
        state.unknown || state.pending != 0
    }
}

pub(super) struct DispatchGuard {
    ancestors: Vec<Arc<Effects>>,
    complete: bool,
}

impl DispatchGuard {
    pub fn begin(ancestors: Vec<Arc<Effects>>) -> Self {
        for ancestor in &ancestors {
            ancestor.0.lock().pending += 1;
        }
        Self {
            ancestors,
            complete: false,
        }
    }

    pub fn completed(mut self) {
        self.complete = true;
    }
}

impl Drop for DispatchGuard {
    fn drop(&mut self) {
        for ancestor in &self.ancestors {
            let mut state = ancestor.0.lock();
            state.pending -= 1;
            state.unknown |= !self.complete;
        }
    }
}

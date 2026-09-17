//! Live display authority: a sequence fence over advisory output.
//!
//! Interruption is a durable `Cancel` in the session log; there is no worker
//! generation id left to isolate live output by. A client's own follow loop
//! is the only thing that can observe a `Cancel` early — from durable
//! history, before a stale advisory queued ahead of it drains — so the fence
//! it keeps is a sequence, not an identity: [`LiveEventState::accept_interrupt`]
//! records the `Cancel`'s log sequence, and [`LiveEventState::should_render`]
//! drops any advisory that predates it.
//!
//! Stop caches only ever reject live/advisory output; they never authorize or
//! gate durable output, which the session log already settles on its own.
use super::AdvisoryEnvelope;
use parking_lot::RwLock;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

#[derive(Clone, Default, Debug)]
pub struct LiveEventState {
    attached: Arc<RwLock<Attachment>>,
    cancel_seq: Arc<AtomicU64>,
}

/// Per-attachment state, distinct from the cancel fence: a `fork` always
/// gets a fresh one, while every clone of one `LiveEventState` (via `Clone`)
/// shares it.
#[derive(Default, Debug)]
struct Attachment {
    retired: bool,
}

impl LiveEventState {
    /// A new attachment cannot be changed by the previous attachment's reader
    /// (`same_attachment` is `Arc` identity, and this allocates a fresh one),
    /// but it keeps the same cancel fence: an interrupt already observed
    /// still applies to whatever this attachment goes on to read.
    pub fn fork(&self) -> Self {
        Self {
            attached: Default::default(),
            cancel_seq: Arc::clone(&self.cancel_seq),
        }
    }

    /// A detached reader must never re-arm its queue after the UI replaces
    /// it with a fresh attachment. Every clone of this `LiveEventState`
    /// shares the same `Attachment`, so this reaches all of them even though
    /// none of them changed identity.
    pub fn retire(&self) {
        self.attached.write().retired = true;
    }

    pub fn same_attachment(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.attached, &other.attached)
    }

    /// Drop everything emitted before `cancel_seq`: the sequence a `Cancel`
    /// landed at in the session log, as observed by the client replaying
    /// durable history. Monotonic — an older observation can never lower a
    /// fence a later one already raised.
    pub fn accept_interrupt(&self, cancel_seq: u64) {
        self.cancel_seq.fetch_max(cancel_seq, Ordering::Relaxed);
    }

    /// Whether an advisory should be rendered: its `after_seq` must clear
    /// both the client's last-applied durable sequence and the latest
    /// accepted interrupt. A retired attachment renders nothing at all.
    pub fn should_render(&self, envelope: &AdvisoryEnvelope, last_durable_seq: u64) -> bool {
        if self.attached.read().retired {
            return false;
        }
        let fence = last_durable_seq.max(self.cancel_seq.load(Ordering::Relaxed));
        envelope.after_seq >= fence
    }
}

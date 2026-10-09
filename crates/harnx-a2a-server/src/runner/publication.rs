//! Direct admission delivery cache. Every record comes from committed authority.
use super::A2aEvent;
use crate::store::TaskRecord;
use std::sync::Arc;
use tokio::sync::broadcast;

#[derive(Default)]
pub(super) struct LiveState {
    pub(super) record: Option<Arc<TaskRecord>>,
}
impl LiveState {
    pub(super) fn snapshot(&self) -> Option<TaskRecord> {
        self.record.as_deref().cloned()
    }
}
pub(super) struct StreamChannels {
    pub(super) events: broadcast::Sender<A2aEvent>,
    pub(super) live: Arc<parking_lot::Mutex<LiveState>>,
}

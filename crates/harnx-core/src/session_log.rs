use crate::session::SessionLogEntry;

use anyhow::Result;

pub trait SessionLog {
    fn append_event(&mut self, entry: &SessionLogEntry) -> Result<u64>;
    fn load_events(&self) -> Result<Vec<(u64, SessionLogEntry)>>;
    fn replay_from(&self, seq: u64) -> Result<Vec<SessionLogEntry>>;
}

//! Bounded retention of finished processes and their cached diffs.
use super::SpawnedProcess;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

const EXITED_PROCESS_TTL: Duration = Duration::from_secs(30 * 60);
const MAX_EXITED_PROCESSES: usize = 128;
const MAX_EVICTED_PROCESS_IDS: usize = 1024;

pub(super) struct SpawnedProcesses {
    pub(super) entries: HashMap<String, Arc<SpawnedProcess>>,
    /// Recent evictions only. Once forgotten, an ID gets the unknown-ID error.
    evicted: VecDeque<String>,
    pub(super) exited_ttl: Duration,
    pub(super) max_exited: usize,
    pub(super) max_evicted_ids: usize,
}

impl Default for SpawnedProcesses {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            evicted: VecDeque::new(),
            exited_ttl: EXITED_PROCESS_TTL,
            max_exited: MAX_EXITED_PROCESSES,
            max_evicted_ids: MAX_EVICTED_PROCESS_IDS,
        }
    }
}

impl SpawnedProcesses {
    /// Called on registry access and tool dispatch, not just waits. `now` is
    /// explicit so tests can advance retention without sleeping.
    pub(super) fn evict(&mut self, now: Instant) {
        let mut exited: Vec<_> = self
            .entries
            .iter()
            .filter_map(|(id, entry)| {
                // A failed supervisor wait doesn't prove the process exited.
                entry
                    .exit
                    .borrow()
                    .as_ref()?
                    .exited_at
                    .map(|at| (at, id.clone()))
            })
            .collect();
        exited.sort_unstable();
        let excess = exited.len().saturating_sub(self.max_exited);
        for (index, (at, id)) in exited.into_iter().enumerate() {
            if index >= excess && now.saturating_duration_since(at) < self.exited_ttl {
                continue;
            }
            self.entries.remove(&id);
            self.evicted.push_back(id);
        }
        while self.evicted.len() > self.max_evicted_ids {
            self.evicted.pop_front();
        }
    }

    pub(super) fn was_evicted(&self, execution_id: &str) -> bool {
        self.evicted.iter().any(|id| id == execution_id)
    }
}

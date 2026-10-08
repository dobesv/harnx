//! Live snapshot state, accessed under the runner's publication lock.
use super::{event_map::artifact, A2aEvent};
use crate::store::TaskRecord;
use a2a_lf::{Part, PartContent, StreamResponse, TaskArtifactUpdateEvent};
use std::sync::Arc;
use tokio::sync::broadcast;

#[derive(Default)]
pub(super) struct LiveState {
    pub(super) record: Option<Arc<TaskRecord>>,
    pub(super) sequence: u64,
    // Append published deltas in place; materialize protocol artifacts only on reads.
    artifact_text: Option<String>,
}

impl LiveState {
    pub(super) fn snapshot(&self) -> Option<TaskRecord> {
        let mut snapshot = self.record.as_deref()?.clone();
        snapshot.stream_seq = self.sequence;
        if let Some(text) = &self.artifact_text {
            snapshot.task.artifacts = Some(vec![artifact(text.clone())]);
        }
        Some(snapshot)
    }
}

pub(super) struct StreamChannels {
    pub(super) events: broadcast::Sender<A2aEvent>,
    pub(super) live: Arc<parking_lot::Mutex<LiveState>>,
}

impl LiveState {
    pub(super) fn apply_response(&mut self, response: &StreamResponse) {
        if let StreamResponse::ArtifactUpdate(update) = response {
            self.apply_artifact(update);
        }
    }

    fn apply_artifact(&mut self, update: &TaskArtifactUpdateEvent) {
        let text = self.artifact_text.get_or_insert_with(String::new);
        if update.append != Some(true) {
            text.clear();
        }
        append_text(text, &update.artifact.parts);
    }
}

fn append_text(text: &mut String, parts: &[Part]) {
    for part in parts {
        if let PartContent::Text(delta) = &part.content {
            text.push_str(delta);
        }
    }
}

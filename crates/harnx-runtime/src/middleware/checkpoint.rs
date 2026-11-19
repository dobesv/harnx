use super::{
    frame::Frame,
    resume::{AcceptedInteraction, CheckpointState, FrameCheckpoint, FrameIdentity},
};
use anyhow::{anyhow, ensure, Result};
use harnx_engine::tool::{MaterializedToolResult, ToolCallCheckpoint};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

/// JSON stash per frame; private snapshots and all frame metadata per call.
pub(super) const MAX_STASH_BYTES: usize = 256 * 1024;
const MAX_CHECKPOINT_BYTES: usize = 8 * 1024 * 1024;

pub(super) struct ResumeState {
    pub state: CheckpointState,
    pub snapshots: HashMap<String, MaterializedToolResult>,
    pub responses: Vec<AcceptedInteraction>,
}

impl ResumeState {
    pub fn new(mut state: CheckpointState) -> Result<Self> {
        let (checkpoint, responses) = state.restore()?;
        if !checkpoint.state.is_null() {
            state = serde_json::from_value(checkpoint.state)?;
        }
        let resume = Self {
            state,
            snapshots: checkpoint.snapshots,
            responses,
        };
        resume.checkpoint()?;
        Ok(resume)
    }

    pub fn frame(&self, identity: &FrameIdentity) -> FrameCheckpoint {
        self.state
            .frames
            .iter()
            .find(|frame| frame.matches(identity))
            .cloned()
            .unwrap_or_else(|| FrameCheckpoint {
                middleware: identity.middleware.clone(),
                step_path: identity.step_path.clone(),
                ..Default::default()
            })
    }

    pub fn record(
        &mut self,
        frame: FrameCheckpoint,
        snapshots: HashMap<String, MaterializedToolResult>,
    ) {
        self.state
            .frames
            .retain(|old| old.middleware != frame.middleware || old.step_path != frame.step_path);
        self.state.frames.push(frame);
        self.snapshots.extend(snapshots);
        let retained: HashSet<_> = self
            .state
            .frames
            .iter()
            .flat_map(|frame| frame.retained.values().flatten())
            .collect();
        self.snapshots
            .retain(|reference, _| retained.contains(reference));
    }

    fn validate_retained(&self) -> Result<()> {
        for frame in &self.state.frames {
            ensure!(
                serde_json::to_vec(&frame.stash)?.len() <= MAX_STASH_BYTES,
                "middleware stash exceeds 256 KiB"
            );
            for (key, references) in &frame.retained {
                ensure!(
                    frame.stash.contains_key(key),
                    "retained middleware snapshot has no stash entry"
                );
                for reference in references {
                    ensure!(
                        self.snapshots.contains_key(reference),
                        "missing retained middleware snapshot"
                    );
                }
            }
        }
        Ok(())
    }

    pub fn checkpoint(&self) -> Result<ToolCallCheckpoint> {
        self.validate_retained()?;
        let state = serde_json::to_value(&self.state)?;
        let mut bytes = serde_json::to_vec(&state)?.len();
        for snapshot in self.snapshots.values() {
            // ToolResult's public serde deliberately skips private observations.
            bytes = bytes.saturating_add(
                serde_json::to_vec(&(&snapshot.result, &snapshot.result.execution_context))?.len(),
            );
        }
        ensure!(
            bytes <= MAX_CHECKPOINT_BYTES,
            "middleware checkpoint exceeds 8 MiB"
        );
        Ok(ToolCallCheckpoint {
            state,
            snapshots: self.snapshots.clone(),
        })
    }
}

impl Frame {
    pub fn save(&self, key: String, value: Value) -> Result<Value> {
        ensure!(!key.is_empty(), "stash key must not be empty");
        let mut state = self.state.lock();
        let refs = state.snapshots.references(&value)?;
        let mut stash = state.stash.clone();
        stash.insert(key.clone(), value);
        ensure!(
            serde_json::to_vec(&stash)?.len() <= MAX_STASH_BYTES,
            "middleware stash exceeds 256 KiB"
        );
        state.stash = stash;
        state.retained.insert(key, refs);
        Ok(Value::Null)
    }

    pub fn saved(&self, key: &str) -> Result<Option<Value>> {
        let Some(value) = self.restored.stash.get(key).cloned() else {
            return Ok(None);
        };
        let mut state = self.state.lock();
        let resume = self.resume.lock();
        for reference in self.restored.retained.get(key).into_iter().flatten() {
            let snapshot = resume
                .snapshots
                .get(reference)
                .ok_or_else(|| anyhow!("missing retained middleware snapshot"))?;
            state.snapshots.restore(reference.clone(), snapshot.clone());
        }
        Ok(Some(value))
    }

    pub fn checkpoint_frame(&self) -> Result<()> {
        let state = self.state.lock();
        let frame = FrameCheckpoint {
            stash: state.stash.clone(),
            retained: state.retained.clone(),
            ..self.restored.clone()
        };
        let mut snapshots = self.resume.lock().snapshots.clone();
        snapshots.extend(state.snapshots.retained(frame.retained.values().flatten()));
        drop(state);
        self.resume.lock().record(frame, snapshots);
        Ok(())
    }
}

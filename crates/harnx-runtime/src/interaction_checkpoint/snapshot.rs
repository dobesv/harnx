//! Explicit serialization preserves fields omitted by public ToolResult serde.
#[cfg(test)]
#[path = "snapshot_tests.rs"]
mod tests;

use anyhow::{ensure, Result};
use harnx_core::{execution_context::ExecutionContextObservation, tool::ToolResult};
use harnx_engine::tool::{InnerToolCallStatus, MaterializedToolResult, ToolCallCheckpoint};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultSnapshot {
    pub version: u32,
    pub result: ToolResult,
    pub execution_context: Option<ExecutionContextObservation>,
    pub status: SnapshotStatus,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotStatus {
    Ok,
    ToolError,
    Blocked,
}

impl ResultSnapshot {
    pub fn new(materialized: &MaterializedToolResult) -> Self {
        Self {
            version: 1,
            result: materialized.result.clone(),
            execution_context: materialized.result.execution_context.clone(),
            status: match materialized.status {
                InnerToolCallStatus::Ok => SnapshotStatus::Ok,
                InnerToolCallStatus::ToolError => SnapshotStatus::ToolError,
                InnerToolCallStatus::Blocked => SnapshotStatus::Blocked,
            },
        }
    }

    pub fn restore(mut self) -> Result<MaterializedToolResult> {
        ensure!(
            self.version == 1,
            "unsupported private result snapshot version"
        );
        self.result.execution_context = self.execution_context;
        Ok(MaterializedToolResult::new(
            self.result,
            match self.status {
                SnapshotStatus::Ok => InnerToolCallStatus::Ok,
                SnapshotStatus::ToolError => InnerToolCallStatus::ToolError,
                SnapshotStatus::Blocked => InnerToolCallStatus::Blocked,
            },
        ))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallCheckpoint {
    pub version: u32,
    pub state: serde_json::Value,
    pub snapshots: HashMap<String, ResultSnapshot>,
}

impl CallCheckpoint {
    pub fn new(checkpoint: &ToolCallCheckpoint) -> Self {
        Self {
            version: 1,
            state: checkpoint.state.clone(),
            snapshots: checkpoint
                .snapshots
                .iter()
                .map(|(key, snapshot)| (key.clone(), ResultSnapshot::new(snapshot)))
                .collect(),
        }
    }

    pub fn restore(self) -> Result<ToolCallCheckpoint> {
        ensure!(
            self.version == 1,
            "unsupported private call checkpoint version"
        );
        Ok(ToolCallCheckpoint {
            state: self.state,
            snapshots: self
                .snapshots
                .into_iter()
                .map(|(key, value)| Ok((key, value.restore()?)))
                .collect::<Result<_>>()?,
        })
    }
}

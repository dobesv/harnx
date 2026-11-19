//! Integration tests for advertising and answering blocked direct sub-agents (T2.6 Part A).

#[path = "subagent_blocked/fixture.rs"]
pub(crate) mod fixture;

#[path = "subagent_blocked/async_cases.rs"]
mod async_cases;

#[path = "subagent_blocked/sync_cases.rs"]
mod sync_cases;

#[path = "subagent_blocked/persistence_cases.rs"]
mod persistence_cases;

#[path = "subagent_blocked/hierarchy_cases.rs"]
mod hierarchy_cases;

#[path = "subagent_blocked/live_progress_cases.rs"]
mod live_progress_cases;

#[path = "subagent_blocked/serde_cases.rs"]
mod serde_cases;

#[path = "subagent_blocked/parent_flow.rs"]
mod parent_flow;

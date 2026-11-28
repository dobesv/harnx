//! Reporting on how agents use the script-mode `bash` tool, built from stored
//! session logs.

mod collect;
mod merge;
#[cfg(test)]
mod nats_tests;
mod report;
#[cfg(test)]
mod report_tests;
mod session;
#[cfg(test)]
mod session_tests;
mod trace;

pub use collect::{agent_matcher, collect_script_usage, collect_session_script_usage};
pub use report::{
    CallsPerScript, FailureRef, ScriptUsageReport, ScriptUsageScope, SessionScriptUsageDetail,
    ToolShare, TurnGroup, UsageTotals, FAILURE_LIST_LIMIT, INLINE_AGENT_LABEL,
};
pub use session::{FailedCall, FailureKind, ScriptRun, TimeWindow};
pub use trace::TraceRow;
// The bash provider's tests parse its rendered report back through this.
#[cfg(test)]
pub(crate) use trace::parse_report;

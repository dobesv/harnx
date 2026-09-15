//! Durable, generation-scoped execution ownership and downward cancellation.
//!
//! # Logical interruption versus physical cleanup (#1878, Stage 1)
//!
//! `StopDecision` is persisted in the same operation CAS that accepts an
//! interruption. Its presence means `LogicalState::Interrupted`: an irreversible
//! stop fence for work/output and immediate eligibility for a new generation.
//! `CleanupState` separately reports owner/child shutdown, never transcript
//! projection progress (`cancel_recorded`). Abandonment does not confirm cleanup.
//!
//! The dimensions are additive views over the persisted stop decision and the
//! existing lifecycle facts, rather than duplicate mutable state. `OperationState`
//! retains the cleanup/projection lifecycle; `request_cancel` bridges registered
//! operations to gate acceptance. Legacy `Cancelled` is not a gate stop receipt.
//! Use `can_replace_generation` for replacement, `allows_continuation` for existing
//! descendants, `accepts_work` for new admissions, `cleanup_confirmed` for physical
//! shutdown, and `can_prune` for physical-record retirement. Never broaden the
//! compatibility `OperationState::is_terminal` to include logical interruption.
//!
//! Pruning CAS-replaces physical records with compact retired records on the SAME
//! generation key. They retain identity, parent lineage, and stop decisions until
//! session deletion, including after the current-generation pointer advances.
//! This avoids an archive/delete gap and prevents reuse of a retired generation.
//! `ExecutionStore::stop_decision` follows active or retired lineage; a missing
//! ancestor is an error, not evidence that replay/output is allowed.
//!
//! The [`gate`] module implements Stage 2's opt-in, tree-wide CAS commit ledger.
//! The Stage 1 APIs above retain their compatibility behavior and must not be
//! mixed with gate-native output. KV is authoritative; Core NATS only wakes
//! owners sooner. A negative stop read never authorizes a subsequent write.
//! Stage 3 wires tool journals/cache/replay and an activation-marker bridge into
//! this gate. Stages 4-6 fence transcript/recovery/live events. Stage 7 adds
//! [`CleanupStatus`] and worker-lifetime reconciliation; cleanup reports use the
//! same gate and never authorize output. Stage 8 frontends return on root acceptance.

pub mod gate;
pub use gate::*;

mod cleanup;
pub use cleanup::CleanupStatus;
mod tasks;
pub use tasks::CleanupTasks;
mod model;
mod state;
mod store;
pub use model::*;
pub use state::*;
pub use store::{ExecutionStore, RecoveryHistory};

mod telemetry;

#[cfg(test)]
#[path = "../../harnx-runtime/tests/common/mod.rs"]
mod test_common;

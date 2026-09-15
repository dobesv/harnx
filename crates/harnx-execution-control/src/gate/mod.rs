//! Per-tree commit ledger (#1878, Stage 2).
//!
//! `open_gate` starts an opt-in, gate-native tree. `StartWork` registers children;
//! generation installation, owner changes, stops and exact output all serialize
//! on `sessions/{root.session_id}/gates/{root.execution_id}/head`. Immutable
//! crit-bit index paths and candidate decisions are written first. Only root CAS
//! commits them. A stale head can produce garbage but cannot authorize output.
//! Even an idempotent positive retry CAS-validates its admission snapshot.
//!
//! This ledger is the logical authority for opted-in trees, NOT a read-through
//! cache of the legacy physical Operation graph. GET-based imports or separately
//! updated lease/owner records would reintroduce the race. All lease handovers
//! must revoke/replace the gate owner through this boundary before new work is
//! admitted. `Owner::fence` is a monotonic attempt token, not a lease TTL check.
//! Stage 3 uses a physical activation CAS marker to bring tool invocation lineage
//! into this authority. `cancel_operation` then joins the same gate; Stage 1
//! `accept_interrupt` rejects marked operations. Full lease-expiry, deletion and
//! transcript adapters remain later stages.
//!
//! Action identity includes its exact context and serialized content. Receipts
//! prove committed history, never generic permission to append later. A completed
//! producer's reply still needs a fresh `ConsumeReply` under the receiving context.
//! `ProjectCommitted` acknowledges projection of an exact historical commit using
//! a conditional cursor; sink adapters must use durable commit-ID/conditional
//! append recovery, not JetStream's time-limited dedup or a cursor alone.
//!
//! Checkpoint CAS switches epochs, fencing every old staged candidate before GC.
//! Each KV value is bounded (128 KiB); state updates copy index paths rather than
//! append to an ever-growing head. Committed payloads and idempotency/stop proofs
//! remain until session deletion while projector/replay retention rules are not
//! yet installed. GC removes obsolete state paths and uncommitted candidates.
//! Session IDs cannot migrate between gate authorities without a future explicit
//! handover protocol. Authorization to invoke this control-plane API is the
//! caller's responsibility; an ExecutionContext is identity, not a security secret.

mod actions;
mod bridge;
pub use bridge::GateRegistration;
mod checkpoint;
mod cleanup;
pub use cleanup::CleanupScope;
mod engine;
mod index;
mod ledger;
mod payload;
mod state;
mod types;
pub use types::*;

#[cfg(test)]
mod tests;

//! Durable, generation-scoped execution ownership and downward cancellation.
//!
//! KV is authoritative. Core NATS messages only wake owners sooner. No caller
//! may start work before registering it, or report cancellation before its
//! owner and registered children have stopped.

mod model;
mod store;
pub use model::*;
pub use store::ExecutionStore;

mod telemetry;

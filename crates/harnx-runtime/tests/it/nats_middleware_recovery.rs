//! Crash-injection integration tests for in-process tool middleware recovery across NATS.

pub(crate) use super::nats_middleware_interaction::harness;

#[path = "nats_middleware_recovery/matrix_cases.rs"]
mod matrix_cases;

#[path = "nats_middleware_recovery/edge_cases.rs"]
mod edge_cases;

#[path = "nats_middleware_recovery/boundary_cases.rs"]
mod boundary_cases;

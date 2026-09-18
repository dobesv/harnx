//! Verification suite for log-fenced interruption, end to end against a real
//! `nats-server`: what a `Cancel` rejects, what wind-up owes the log
//! afterwards, and what a tool round costs.

mod common;
#[allow(dead_code)]
#[path = "common/worker.rs"]
mod worker;

#[path = "interruption/mod.rs"]
mod interruption;

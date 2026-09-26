//! End-to-end ACP/NATS tests organized by protocol responsibility.

#[path = "nats_integration/cancellation.rs"]
mod cancellation;
#[path = "nats_integration/handoff.rs"]
mod handoff;
#[path = "nats_integration/lifecycle.rs"]
mod lifecycle;
#[path = "nats_integration/listing.rs"]
mod listing;
#[path = "nats_integration/permission.rs"]
mod permission;
#[path = "nats_integration/persistence.rs"]
mod persistence;
#[path = "nats_integration/prompt.rs"]
mod prompt;
#[path = "nats_integration/support/mod.rs"]
mod support;

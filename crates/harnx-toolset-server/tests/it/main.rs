//! All of this crate's integration tests, built as one binary. A separate
//! binary per file links the whole dependency graph again for each, which
//! dominated build time; nextest still runs every test in its own process.

mod common;

mod aggregate_server;
mod cancellation;
mod completion;
mod fencing;
mod filter_integration;
mod filter_unit;
mod name_unit;
mod nats_server;
mod progress;
mod replay;
mod reply_projection;

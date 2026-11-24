//! Native-mode per-user credentials tests, run in-process against a real NATS
//! server and a per-user mock upstream MCP server (plan T5 acceptance list).

#![cfg(test)]

mod connect;
mod expiry;
mod fencing;
pub mod fixture;
mod http;
mod lifecycle;
mod pool;
mod security;

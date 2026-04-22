//! ACP (Agent Client Protocol) client glue and shared types for harnx.
//!
//! This crate contains protocol-level code: the `AcpClient` that speaks
//! the ACP wire protocol with a sub-process, the `AcpServerConfig` YAML
//! schema, and the `NestedAcpEvent` enum that carries chunk updates
//! between the client and downstream consumers.
//!
//! Harnx-specific orchestration (`AcpManager` with CLI spinner display)
//! and the full ACP server implementation (`HarnxAgent` — depends on the
//! harnx config, LLM pipeline, tool registry) stay in the `harnx` crate.

mod client;
mod config;
mod event;

pub use client::AcpClient;
pub use config::AcpServerConfig;
pub use event::NestedAcpEvent;

//! Shared domain types, event model, and pure utilities used across the
//! harnx workspace. See the spec at
//! `docs/superpowers/specs/2026-04-19-monorepo-refactor-design.md` for the
//! role this crate plays in the multi-crate split.

pub mod abort;
pub mod agent_config;
pub mod api_types;
pub mod cli;
pub mod config_paths;
pub mod context;
pub mod crypto;
pub mod error;
pub mod event;
pub mod hooks;
pub mod input;
pub mod last_message;
pub mod macros;
pub mod message;
pub mod model;
pub mod path;
pub mod provider_config;
pub mod retry_config;
pub mod session;
pub mod sink;
pub mod system_vars;
pub mod text;
pub mod tool;
pub mod working_mode;

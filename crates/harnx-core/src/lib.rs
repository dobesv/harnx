//! Shared domain types, event model, and pure utilities used across the
//! harnx workspace. See the spec at
//! `docs/superpowers/specs/2026-04-19-monorepo-refactor-design.md` for the
//! role this crate plays in the multi-crate split.

pub mod abort;
pub mod cli;
pub mod context;
pub mod crypto;
pub mod error;
pub mod event;
pub mod hooks;
pub mod path;

//! Shared test-only helpers for the config module's test submodules.
#![cfg(test)]

pub(super) use crate::test_environment::{env_lock, env_lock_async, EnvGuard};

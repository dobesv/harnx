//! Test utilities for end-to-end and integration tests.
//!
//! This module provides shared helpers for test-only infrastructure, including
//! mock clients, TUI harnesses, synchronization primitives, and ACP test
//! servers. Implementations are added in follow-up tasks.

mod mock_client;

pub use mock_client::*;

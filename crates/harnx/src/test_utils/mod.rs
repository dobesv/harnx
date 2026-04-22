//! Test utilities for end-to-end and integration tests.
//!
//! This module provides shared helpers for test-only infrastructure, including
//! mock clients, TUI harnesses, synchronization primitives, and ACP test servers.
//!
//! # Overview
//!
//! The test utilities are organized into several modules:
//!
//! - [`mock_client`] - Mock LLM client for simulating streaming responses and tool calls
//! - [`tui_harness`] - Test harness for TUI rendering tests without a real terminal
//! - [`tmux_harness`] - Minimal tmux-backed harness for driving the real TUI locally
//! - [`sync`] - Synchronization primitives for async test coordination
//! - [`mock_acp`] - Mock ACP server for testing sub-agent delegation
//!
//! # Quick Start
//!
//! ## Mock Client Example
//!
//! ```ignore
//! use harnx::test_utils::{MockClient, MockTurnBuilder};
//!
//! // Create a mock that streams text
//! let mock = MockClient::builder()
//!     .add_turn(
//!         MockTurnBuilder::new()
//!             .add_text_chunk("Hello")
//!             .add_text_chunk(" world!")
//!             .build()
//!     )
//!     .build();
//! ```
//!
//! ## TUI Test Example
//!
//! ```ignore
//! use harnx::test_utils::TuiTestHarness;
//!
//! let mut harness = TuiTestHarness::with_size(80, 24);
//! harness.render();
//! let screen = harness.screen_contents();
//! assert!(screen.contains("Input"));
//! ```
//!
//! ## Synchronization Example
//!
//! ```ignore
//! use harnx::test_utils::SyncHarness;
//! use std::time::Duration;
//!
//! let harness = SyncHarness::new()
//!     .with_poll_interval(Duration::from_millis(10));
//!
//! // Wait for a condition
//! harness.wait_until_screen_contains("expected", Duration::from_secs(5), || {
//!     screen_contents.clone()
//! }).await?;
//! ```

pub mod interrupt;
pub mod mock_acp;
pub mod mock_openai_server;
pub mod tmux_harness;

pub use harnx_runtime::test_utils::mock_client::*;
pub use harnx_runtime::test_utils::sync::*;
pub use harnx_runtime::test_utils::{mock_client, sync};

pub use harnx_tui::test_utils::TuiTestHarness;
pub use interrupt::*;
pub use mock_acp::*;
pub use mock_openai_server::*;
pub use tmux_harness::*;

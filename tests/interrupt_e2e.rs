//! End-to-end tests that characterise interrupt handling (Ctrl-C, SIGINT,
//! and ACP `session/cancel`) across TUI, one-shot, and ACP-server modes.
//!
//! Tests that currently fail against `main` are marked with
//! `#[ignore = "pending interrupt fix (#292)"]`. Run the full suite with
//!   cargo nextest run --test interrupt_e2e --run-ignored=all
//! to see the pre-fix baseline. As fixes land, individual tests are
//! un-ignored in the same PR.

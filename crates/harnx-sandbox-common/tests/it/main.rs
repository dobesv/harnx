//! This crate's integration tests, built as one binary. `macos_tty.rs` stays
//! a binary of its own: it sandboxes the test process itself, which can't be
//! undone.

mod sandbox_exec_env;

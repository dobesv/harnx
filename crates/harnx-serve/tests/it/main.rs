//! All of this crate's integration tests, built as one binary. A separate
//! binary per file links the whole dependency graph again for each, which
//! dominated build time; nextest still runs every test in its own process.

#[allow(dead_code)]
mod support;

mod ag_ui_control_plane;
mod ag_ui_remote_follow;
mod nats_attention_repair;
mod nats_mark_read_unread;
mod nats_sigterm_drain_e2e;
mod nats_sse_read_gate;

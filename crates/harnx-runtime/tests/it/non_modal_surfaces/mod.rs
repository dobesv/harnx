//! Non-modal A2UI surface integration tests (plan T5.1 Part A).
//!
//! A real worker daemon runs a session whose model stub issues tool calls;
//! the tool server publishes a user-audience A2UI surface on its completed
//! result. Tests cover each visibility level, direct tool actions, model-input
//! exclusion, non-blocking publication, and rejection-without-side-effects.

mod envelope;
mod fixture;
mod helpers;
mod stub;

mod at_most_once;
mod basics;
mod client_ack;
mod direct_tool;
mod lease;
mod recovery;

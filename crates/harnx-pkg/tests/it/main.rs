//! All of this crate's integration tests, built as one binary. A separate
//! binary per file links the whole dependency graph again for each, which
//! dominated build time; nextest still runs every test in its own process.

#[allow(dead_code)]
mod helpers;

mod commands;
mod e2e;
mod git_fetcher;
mod oci_fetcher;

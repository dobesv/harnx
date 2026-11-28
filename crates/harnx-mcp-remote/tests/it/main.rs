//! All of this crate's integration tests, built as one binary. A separate
//! binary per file links the whole dependency graph again for each, which
//! dominated build time; nextest still runs every test in its own process.

// The embedded Python VM meters its memory through this allocator and
// refuses to run without it (harnx-script's own tests install the same one).
#[global_allocator]
static VM_ALLOCATOR: harnx_script::VmAllocator = harnx_script::VM_ALLOCATOR;

#[allow(dead_code)]
mod common;

mod auth;
mod headers;
mod lazy_connect;
mod native;
mod native_common;
mod streamable_http;

mod native_oauth;

mod client_credentials;

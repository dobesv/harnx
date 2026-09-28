//! NATS-backed blob storage for attachments and plans.
//!
//! This crate provides data access for `cid:` URLs over NATS:
//! - Media object store operations
//! - Plans KV bucket helper
//! - Activity touch (rate-limited)
//! - Owner deletion
//!
//! ## Crate Layering
//!
//! Tool servers depend on `harnx-blob-store` which depends on:
//! - `harnx-core` (types, no I/O)
//! - `harnx-nats-common` (NATS connection helpers)
//! - `async-nats` (NATS client)
//!
//! `harnx-blob-store` MUST NOT depend on `harnx-runtime` or `harnx-toolset-server`.

mod activity;
mod delete;
pub mod media;
mod plans;

pub use activity::{touch_activity, ActivityGuard};
pub use delete::delete_owner;
pub use plans::{ensure_plans_bucket, PLAN_BUCKET};

use harnx_core::cid_url::{CidUrl, SessionRef};

/// Build a CidUrl for media from a SessionRef and hash.
pub fn media_cid_url(session: &SessionRef, hash: &str) -> CidUrl {
    CidUrl::Media {
        session: session.clone(),
        hash: hash.to_string(),
    }
}

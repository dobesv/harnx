//! Attachment toolset: read and create NATS-backed blobs via cid: URLs.
//!
//! This crate provides two tools for agent access to attachments:
//! - `attachment_read`: Read media blobs and rendered plans with truncation support
//! - `attachment_create`: Create text or base64-encoded binary attachments
//!
//! Both tools operate only over NATS JetStream. No filesystem or HTTP access.

pub mod handlers;
mod script_result;
mod script_schema;
mod toolset;

pub use script_result::ResultStyle;
pub use toolset::AttachmentToolset;

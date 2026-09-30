//! Attachment toolset: read and create NATS-backed blobs via cid: URLs.
//!
//! This crate provides two tools for agent access to attachments:
//! - `attachment_read`: Read media blobs and rendered plans with truncation support
//! - `attachment_create`: Create new text attachments
//!
//! Both tools operate only over NATS JetStream. No filesystem or HTTP access.

pub mod handlers;
mod toolset;

pub use toolset::AttachmentToolset;

#![cfg(test)]
//! Integration tests for OAuth acquisition, refresh, and connection fencing in native mode.

pub mod acquisition;
pub mod fixture;
pub use fixture::*;
pub mod leakage;
pub mod lifecycle;
pub mod refresh;
pub mod security;
pub mod validation;

pub use crate::native_common::*;
pub use base64::engine::general_purpose::STANDARD;
pub use base64::Engine;
pub use harnx_mcp_remote::native::toolset::RemoteCredentialsToolset;
pub use harnx_toolset::ToolInvokeError;
pub use serde_json::{json, Value};
pub use std::sync::atomic::Ordering;
pub use std::sync::Arc;
pub use std::time::Duration;
pub use tokio_util::sync::CancellationToken;
pub use url::Url;

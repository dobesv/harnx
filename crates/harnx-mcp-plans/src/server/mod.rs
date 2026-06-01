//! Plans MCP server implementation.
//!
//! Stores plans under per-plan directories using YAML front matter + markdown body.
//! Layout: `<data-dir>/<plan>/plan.md`, `<data-dir>/<plan>/tasks/<id>.md`, and
//! `<data-dir>/<plan>/notes/<id>.md`.

use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, ErrorData, Implementation, ListToolsResult,
    Meta, PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::schemars::{generate::SchemaGenerator, JsonSchema, Schema};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::ServerHandler;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use similar::{ChangeTag, TextDiff};
use std::borrow::Cow;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

mod handler;
mod handlers;
mod params;
mod store;
#[cfg(test)]
mod tests;

pub(crate) use params::*;
pub use store::cleanup_loop;
pub(crate) use store::*;

pub struct PlansServer {
    dir: PathBuf,
}

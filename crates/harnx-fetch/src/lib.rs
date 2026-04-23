//! Web-fetch + document-loading infrastructure for harnx. Moved from
//! `crates/harnx/src/utils/{loader,request}.rs` + the loader-command
//! subset of `utils/command.rs` in Plan 44b (2026-04-22). See
//! `docs/superpowers/specs/2026-04-21-frontend-crate-splits-design.md`.

#[macro_use]
extern crate log;

pub mod command;
pub mod loader;
pub mod request;
mod utils;

pub use command::{run_command, run_command_with_output, run_loader_command};
pub use loader::{
    is_loader_protocol, load_file, load_protocol_path, load_recursive_url, load_url,
    DocumentMetadata, LoadedDocument, EXTENSION_METADATA,
};
pub use request::{
    crawl_website, fetch, fetch_models, fetch_with_loaders, CrawlOptions, Page, DEFAULT_EXTENSION,
    MEDIA_URL_EXTENSION, RECURSIVE_URL_LOADER, URL_LOADER,
};

//! Shared CLI flags used across every harnx binary. Each binary composes
//! `CommonFlags` via `#[command(flatten)]` and adds its own binary-specific
//! flags on top. These flags are intentionally mirror-images of the
//! `harnx` binary's historical flags so that behavior stays identical when
//! the server binaries are eventually split out.

use std::path::PathBuf;

use clap::Args;

#[derive(Args, Debug, Clone, Default)]
pub struct CommonFlags {
    /// Select a model.
    #[arg(short = 'm', long)]
    pub model: Option<String>,

    /// Use an agent by name.
    #[arg(short = 'a', long)]
    pub agent: Option<String>,

    /// Use a session. When the flag is given without an argument, uses the
    /// temporary session.
    #[arg(short = 's', long, num_args = 0..=1, default_missing_value = "")]
    pub session: Option<Option<String>>,

    /// Use a prompt by name.
    #[arg(short = 'p', long)]
    pub prompt: Option<String>,

    /// Enable specific tools.
    #[arg(short = 't', long)]
    pub tool: Vec<String>,

    /// Use a RAG index by name.
    #[arg(short = 'r', long)]
    pub rag: Option<String>,

    /// Override the config root.
    #[arg(long)]
    pub config_path: Option<PathBuf>,

    /// Override the MCP root directory.
    #[arg(long)]
    pub mcp_root: Option<PathBuf>,

    /// Disable all writes (sessions, files, etc.).
    #[arg(long)]
    pub dry_run: bool,

    /// Disable streaming of LLM responses.
    #[arg(long)]
    pub no_stream: bool,
}

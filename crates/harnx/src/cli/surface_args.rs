//! `harnx surface` command-line arguments (plan T5.1 Part B).
//!
//! Split out of `cli.rs` like `interact_args`; the types are re-exported, so
//! `crate::cli::SurfaceArgs` still resolves.

use clap::{Args, Subcommand};

/// Inspect a session's published surfaces or act on one.
#[derive(Args, Debug, PartialEq, Eq)]
pub struct SurfaceArgs {
    #[command(subcommand)]
    pub command: SurfaceSubcommands,
}

#[derive(Subcommand, Debug, PartialEq, Eq)]
pub enum SurfaceSubcommands {
    /// List a session's published surfaces, rendered as text
    List(SurfaceSessionArgs),
    /// Submit an action for a published surface
    Act(SurfaceActArgs),
}

/// An agent session whose surfaces to inspect.
#[derive(Args, Debug, PartialEq, Eq)]
pub struct SurfaceSessionArgs {
    /// Agent that owns the session (e.g., "agent-name" or "agent-name@cluster")
    pub agent: String,
    /// Session ID
    pub session: String,
}

/// Submit an action for a published surface over the durable acceptance path.
///
/// The surface ID and publish sequence come from `harnx surface list`.
/// `--action` names a declared button. Input values are supplied with
/// `--data KEY=VALUE` or `--data-json` and built through the shared T4.2
/// response contract.
#[derive(Args, Debug, PartialEq, Eq)]
pub struct SurfaceActArgs {
    /// Agent that owns the session (e.g., "agent-name" or "agent-name@cluster")
    pub agent: String,
    /// Session ID
    pub session: String,
    /// Surface ID to act on, as shown by `harnx surface list`
    #[arg(long = "id", value_name = "SURFACE_ID")]
    pub surface_id: String,
    /// Publish sequence of the surface; defaults to its only publication and
    /// rejects a mismatch (exact identity, never "act on whatever is pending")
    #[arg(long, value_name = "SEQ")]
    pub publish_seq: Option<u64>,
    /// Declared button action to submit
    #[arg(long, value_name = "ACTION")]
    pub action: String,
    /// Input value, repeatable: KEY=VALUE; a ChoicePicker value is a
    /// comma-separated list of option values or labels
    #[arg(
        long = "data",
        value_name = "KEY=VALUE",
        action = clap::ArgAction::Append,
        conflicts_with = "data_json"
    )]
    pub data: Vec<String>,
    /// JSON object of input values (e.g. '{"env": ["prod"], "name": "rel-1"}')
    #[arg(long, value_name = "JSON", conflicts_with = "data")]
    pub data_json: Option<String>,
    /// Optional note recorded with the action
    #[arg(long, value_name = "TEXT")]
    pub note: Option<String>,
}

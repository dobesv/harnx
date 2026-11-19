//! `harnx interact` command-line arguments.
//!
//! Split out of `cli.rs` to keep that module under the size limit; the types
//! are re-exported, so `crate::cli::InteractAnswerArgs` still resolves.

use clap::{Args, Subcommand};

/// Inspect or answer a session's pending interactions.
#[derive(Args, Debug, PartialEq, Eq)]
pub struct InteractArgs {
    #[command(subcommand)]
    pub command: InteractSubcommands,
}

#[derive(Subcommand, Debug, PartialEq, Eq)]
pub enum InteractSubcommands {
    /// List a session's pending interactions, rendered as text
    List(InteractSessionArgs),
    /// Submit an answer for a pending interaction
    Answer(InteractAnswerArgs),
}

/// An agent session's pending interactions.
#[derive(Args, Debug, PartialEq, Eq)]
pub struct InteractSessionArgs {
    /// Agent that owns the session (e.g., "agent-name" or "agent-name@cluster")
    pub agent: String,
    /// Session ID
    pub session: String,
}

/// Submit an answer for a pending interaction over the durable acceptance path.
///
/// The interaction ID and request sequence come from `harnx interact list`.
/// `--action` names a declared action, or `deny` to dismiss (`approve`/`deny`
/// for confirm interactions). Input values are supplied with `--data
/// KEY=VALUE` or `--data-json` and built through the shared response contract.
#[derive(Args, Debug, PartialEq, Eq)]
pub struct InteractAnswerArgs {
    /// Agent that owns the session (e.g., "agent-name" or "agent-name@cluster")
    pub agent: String,
    /// Session ID
    pub session: String,
    /// Interaction ID to answer, as shown by `harnx interact list`
    #[arg(long = "id", value_name = "INTERACTION_ID")]
    pub interaction_id: String,
    /// Request sequence of the pending request; defaults to the pending one
    /// and rejects a mismatch (exact identity, never "answer whatever is pending")
    #[arg(long, value_name = "SEQ")]
    pub request_seq: Option<u64>,
    /// Action to submit: a declared action name, or `deny` to dismiss;
    /// `approve`/`deny` for confirm interactions
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
    /// JSON object of input values (e.g. '{"choice": ["prod"], "name": "rel-1"}')
    #[arg(long, value_name = "JSON", conflicts_with = "data")]
    pub data_json: Option<String>,
    /// Optional note recorded with the answer
    #[arg(long, value_name = "TEXT")]
    pub note: Option<String>,
}

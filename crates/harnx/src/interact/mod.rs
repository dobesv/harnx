//! CLI inspection and answering of a session's pending interactions.
//!
//! `harnx interact list` renders pending interactions as text; `harnx
//! interact answer` submits one through the same durable acceptance path
//! (`NatsSession::decide_interaction`) every other frontend uses, with the
//! exact interaction ID and request sequence.

mod answer;
mod rerun;
mod submit;

use crate::cli::{InteractArgs, InteractSessionArgs, InteractSubcommands};
use anyhow::{Context, Result};
use harnx_core::interaction::text;
use harnx_runtime::interactions::{derive_pending_interactions, PendingInteraction};
use serde_json::Value;

pub(crate) async fn run_interact_command(args: &InteractArgs) -> Result<u8> {
    match &args.command {
        InteractSubcommands::List(args) => run_list(args).await,
        InteractSubcommands::Answer(args) => answer::run_answer(args).await,
    }
}

/// One pending interaction rendered as text, with its exact identity.
fn pending_text(item: &PendingInteraction) -> String {
    let body = match item.request.kind.as_str() {
        "a2ui" => text::render(&item.request.payload),
        "confirm" => item
            .request
            .payload
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_else(|| item.summary())
            .to_string(),
        "url" => text::render_url(&item.request.payload),
        kind => format!(
            "{}\n\nThis interaction kind ('{kind}') cannot be answered from the CLI.",
            item.summary()
        ),
    };
    format!(
        "Pending interaction {} (kind {}, request sequence {})\n{}",
        item.interaction_id, item.request.kind, item.request_seq, body
    )
}

async fn run_list(args: &InteractSessionArgs) -> Result<u8> {
    let pending = pending_interactions(args).await?;
    if pending.is_empty() {
        println!("No pending interactions.");
        return Ok(0);
    }
    for item in &pending {
        println!("{}", pending_text(item));
        println!();
    }
    println!(
        "Answer with: harnx interact answer {} {} --id <interaction-id> \
         --action <action> [--data KEY=VALUE ...] [--note TEXT]",
        args.agent, args.session
    );
    Ok(0)
}

/// The session's pending interactions, derived from the durable session log.
async fn pending_interactions(args: &InteractSessionArgs) -> Result<Vec<PendingInteraction>> {
    let config = crate::init_frontend_config(crate::config::WorkingMode::Cmd, true).await?;
    let pending = read_pending(&config, &args.agent, &args.session).await?;
    Ok(pending.pending)
}

/// The session's pending interactions plus enough history to classify a
/// re-run of an answer whose request is no longer pending.
pub(super) struct SessionInteractions {
    pending: Vec<PendingInteraction>,
    history: Vec<(u64, harnx_core::session::SessionLogEntry)>,
}

async fn read_pending(
    config: &crate::config::Config,
    agent: &str,
    session: &str,
) -> Result<SessionInteractions> {
    let history = crate::session_attach::session_entries(config, agent, session).await?;
    let pending =
        derive_pending_interactions(&history).context("cannot derive pending interactions")?;
    Ok(SessionInteractions { pending, history })
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

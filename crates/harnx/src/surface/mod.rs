//! CLI inspection of a session's published non-modal surfaces and acting on
//! one (plan T5.1 Part B).
//!
//! `harnx surface list` renders every published surface as text (the shared
//! text fallback) with its exact identity and first-wins resolution; `harnx
//! surface act` submits an action through the same durable path
//! (`NatsSession::submit_surface_action`) every other frontend uses. Nothing
//! here blocks a running turn.

mod act;

use crate::cli::{SurfaceArgs, SurfaceSessionArgs, SurfaceSubcommands};
use anyhow::{bail, Result};
use harnx_core::session::SessionLogEntry;
use harnx_runtime::surfaces::{self, PublishedSurface};

pub(crate) async fn run_surface_command(args: &SurfaceArgs) -> Result<u8> {
    match &args.command {
        SurfaceSubcommands::List(args) => run_list(args).await,
        SurfaceSubcommands::Act(args) => act::run_act(args).await,
    }
}

/// One published surface rendered as text, with its exact identity and
/// first-wins resolution.
fn surface_text(seq: u64, surface: &PublishedSurface, accepted: Option<&str>) -> String {
    let id = surfaces::surface_id_of(&surface.payload).unwrap_or_else(|| "surface".into());
    let body = harnx_core::interaction::text::render(&surface.payload);
    let status = match accepted {
        Some(action) => format!("answered: {action} (first-wins)"),
        None => "outstanding".to_string(),
    };
    format!("Surface {id} (published at sequence {seq}, {status})\n{body}")
}

/// The publication named by `(surface_id, publish_seq)`: exact identity,
/// never "whatever is published". `publish_seq` may be omitted only when the
/// surface ID has exactly one publication.
fn resolve_target(
    entries: &[(u64, SessionLogEntry)],
    surface_id: &str,
    publish_seq: Option<u64>,
) -> Result<(u64, PublishedSurface)> {
    let matching: Vec<(u64, PublishedSurface)> = surfaces::published_surfaces(entries)?
        .into_iter()
        .filter(|(_, surface)| {
            surfaces::surface_id_of(&surface.payload).as_deref() == Some(surface_id)
        })
        .collect();
    if matching.is_empty() {
        bail!(
            "surface '{surface_id}' is not published (unknown); run `harnx surface list` to see \
             published surfaces"
        );
    }
    match publish_seq {
        None => {
            if let [(seq, surface)] = matching.as_slice() {
                return Ok((*seq, surface.clone()));
            }
            bail!(
                "{} publications share surface id '{surface_id}'; pass --publish-seq \
                 (see `harnx surface list`)",
                matching.len()
            );
        }
        Some(seq) => matching
            .into_iter()
            .find(|(candidate, _)| *candidate == seq)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "publish sequence {seq} matches none of the publications of '{surface_id}'"
                )
            }),
    }
}

async fn run_list(args: &SurfaceSessionArgs) -> Result<u8> {
    let config = crate::init_frontend_config(crate::config::WorkingMode::Cmd, true).await?;
    let entries =
        crate::session_attach::session_entries(&config, &args.agent, &args.session).await?;
    let published = surfaces::published_surfaces(&entries)?;
    if published.is_empty() {
        println!("No published surfaces.");
        return Ok(0);
    }
    for (seq, surface) in &published {
        let id = surfaces::surface_id_of(&surface.payload).unwrap_or_default();
        let accepted =
            surfaces::existing_surface_action(&entries, &id, *seq).map(|accepted| accepted.action);
        println!("{}", surface_text(*seq, surface, accepted.as_deref()));
        println!();
    }
    println!(
        "Act with: harnx surface act {} {} --id <surface-id> [--publish-seq N] \
         --action <action> [--data KEY=VALUE ...] [--note TEXT]",
        args.agent, args.session
    );
    Ok(0)
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

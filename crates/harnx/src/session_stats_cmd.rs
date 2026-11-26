//! `harnx info sessions` (#2923 T3): validates the scope, collects T1's
//! typed storage reports, and renders them as compact text or the raw typed
//! JSON. Collection itself is read-only and lives in
//! `harnx_runtime::session_stats`; nothing here connects before the scope
//! has been validated.

use crate::cli::InfoSessionsArgs;
use crate::config::WorkingMode;
use crate::init_frontend_config;
use crate::session_stats_render::{render_agent_text, render_cluster_text, render_session_text};
use anyhow::{bail, Result};
use harnx_runtime::session_stats::{
    self, AgentStatsReport, ClusterStatsReport, SessionStatsDetail,
};

/// The collected report plus its scope, so the render step dispatches on one
/// value instead of re-matching the CLI arguments.
enum StatsView {
    Cluster(ClusterStatsReport),
    Agent(AgentStatsReport),
    Session(SessionStatsDetail),
}

pub async fn run(args: &InfoSessionsArgs) -> Result<u8> {
    // A session id without an agent is rejected at the clap layer; the match
    // below can therefore treat that combination as a programmer error.
    let view = match (&args.agent, &args.session_id) {
        (None, None) => {
            let config = init_frontend_config(WorkingMode::Cmd, true).await?;
            StatsView::Cluster(session_stats::collect_cluster_stats(&config, &args.cluster).await?)
        }
        (Some(agent), None) => {
            let config = init_frontend_config(WorkingMode::Cmd, true).await?;
            StatsView::Agent(
                session_stats::collect_agent_stats(&config, &args.cluster, agent).await?,
            )
        }
        (Some(agent), Some(session_id)) => {
            let config = init_frontend_config(WorkingMode::Cmd, true).await?;
            StatsView::Session(
                session_stats::collect_session_stats(&config, &args.cluster, agent, session_id)
                    .await?,
            )
        }
        (None, Some(_)) => bail!("a session id requires --agent"),
    };
    match view {
        // The typed report is the artifact: print only JSON, no header or
        // trailing chatter, so stdout pipes straight into jq.
        StatsView::Cluster(report) if args.json => print_json(&report)?,
        StatsView::Agent(report) if args.json => print_json(&report)?,
        StatsView::Session(report) if args.json => print_json(&report)?,
        StatsView::Cluster(report) => print!("{}", render_cluster_text(&report)),
        StatsView::Agent(report) => print!("{}", render_agent_text(&report)),
        StatsView::Session(report) => print!("{}", render_session_text(&report)),
    }
    Ok(0)
}

fn print_json<T: serde::Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

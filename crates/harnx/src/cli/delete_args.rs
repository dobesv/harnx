use clap::{Args, Subcommand};

#[derive(Args, Debug, PartialEq, Eq)]
pub struct DeleteArgs {
    #[command(subcommand)]
    pub command: DeleteSubcommands,
}

#[derive(Subcommand, Debug, PartialEq, Eq)]
pub enum DeleteSubcommands {
    /// Delete a remote NATS session log stream + lease key
    Session(DeleteSessionArgs),
    /// Delete remote NATS sessions whose stream has no session metadata and
    /// has not been written for the grace period, after re-checking every
    /// guard (last write, leases, metadata)
    OrphanStreams(DeleteOrphanStreamsArgs),
}

#[derive(Args, Debug, PartialEq, Eq)]
pub struct DeleteSessionArgs {
    /// Agent that owns the session
    #[arg(long)]
    pub agent: String,
    pub session_id: String,
    /// Cluster key from nats_servers/<name>.yaml
    #[arg(long)]
    pub cluster: String,
}

/// Guarded orphan-stream cleanup options for the CLI delete mode.
#[derive(Args, Debug, PartialEq, Eq)]
pub struct DeleteOrphanStreamsArgs {
    /// Cluster key from nats_servers/<name>.yaml
    #[arg(long)]
    pub cluster: String,
    /// A stream must have gone this many hours without a write (since its
    /// creation, if it holds no messages) to be deleted. Defaults to the
    /// configured session retention (cleanup_remote_sessions_days); pass it
    /// explicitly when no retention is configured.
    #[arg(long)]
    pub grace_hours: Option<u64>,
    /// Maximum number of orphan streams deleted in one run
    #[arg(
        long,
        default_value_t = harnx_runtime::session_stream_gc::DEFAULT_MAX_ORPHAN_DELETIONS_PER_RUN
    )]
    pub max: usize,
}

impl DeleteSessionArgs {
    /// An embedded cluster must agree with the explicitly selected deletion cluster.
    pub fn agent_name(&self) -> anyhow::Result<String> {
        let (agent, cluster) = harnx_runtime::config::resolve_session_agent(&self.agent)?;
        if self.agent.contains('@') {
            anyhow::ensure!(
                cluster == self.cluster,
                "Agent cluster '{cluster}' conflicts with --cluster '{}'",
                self.cluster
            );
        }
        Ok(agent)
    }
}

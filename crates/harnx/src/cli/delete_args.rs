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
    /// Delete remote NATS sessions whose effective activity is older than the
    /// retention days, in one bounded, guarded pass (see --dry-run)
    ExpiredSessions(DeleteExpiredSessionsArgs),
}

/// Options for `harnx delete expired-sessions` (#2923). Days resolve from
/// `--days` or the configured `cleanup_remote_sessions_days`; `--max` bounds
/// sessions admitted to the deletion path (including late skips and
/// failures), not just successful deletions. Neither override persists.
#[derive(Args, Debug, PartialEq, Eq)]
pub struct DeleteExpiredSessionsArgs {
    /// Cluster key from nats_servers/<name>.yaml
    #[arg(long)]
    pub cluster: String,
    /// Retention days: candidates have effective activity strictly older than
    /// this. Defaults to the configured cleanup_remote_sessions_days, which
    /// must be set and nonzero; pass it explicitly when it is not.
    #[arg(long)]
    pub days: Option<u64>,
    /// Restrict the pass to one named agent (exact match; inline sessions are
    /// only included when no filter is given)
    #[arg(long)]
    pub agent: Option<String>,
    /// Maximum sessions admitted to the deletion path in this pass, including
    /// attempts that fail or get skipped late; read-only skips do not consume
    /// budget
    #[arg(
        long,
        default_value_t = harnx_runtime::manual_session_cleanup::DEFAULT_MANUAL_CLEANUP_MAX
    )]
    pub max: usize,
    /// Preview candidates and guards without deleting any session state; only
    /// the GC lease's own coordination key is written
    #[arg(long)]
    pub dry_run: bool,
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

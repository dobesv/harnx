use clap::Args;

/// `harnx info script-usage` scope selection. The cluster is always explicit;
/// a single session id additionally requires `--agent` and takes no window.
#[derive(Args, Debug, PartialEq, Eq)]
pub struct InfoScriptUsageArgs {
    /// Cluster key from nats_servers/<name>.yaml
    #[arg(long)]
    pub cluster: String,
    /// Agent name or glob (`pantheon/*`; `*` also crosses `/`); exact name with a session id
    #[arg(long)]
    pub agent: Option<String>,
    /// One session's script detail; requires --agent
    #[arg(requires = "agent")]
    pub session_id: Option<String>,
    /// Count only activity at or after this time: RFC 3339, YYYY-MM-DD (UTC) or a duration ago (7d)
    #[arg(long, conflicts_with = "session_id")]
    pub since: Option<String>,
    /// Count only activity before this time; same forms as --since
    #[arg(long, conflicts_with = "session_id")]
    pub until: Option<String>,
    /// Emit the typed JSON report instead of text
    #[arg(long)]
    pub json: bool,
}

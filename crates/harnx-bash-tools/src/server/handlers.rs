// Auto-split from server.rs for cohesion. See server/mod.rs.
use super::*;

pub(crate) struct ExecPipelineParams<'a> {
    pub(crate) command: &'a str,
    pub(crate) working_dir: Option<&'a str>,
    pub(crate) extra_env: Option<&'a HashMap<String, String>>,
    pub(crate) timeout_secs: Option<u64>,
    pub(crate) truncate_opts: TruncateOpts,
    pub(crate) template_sandbox: Option<TemplateSandbox<'a>>,
}

/// Inputs for building a sandboxed child command. Groups the parameters
/// shared by `build_sandbox_command` to keep its argument count manageable.
#[cfg(unix)]
pub(crate) struct SandboxCommandSpec<'a> {
    pub(crate) working_dir: &'a Path,
    pub(crate) exec_dir: &'a Path,
    pub(crate) command: &'a str,
    pub(crate) extra_env: Option<&'a HashMap<String, String>>,
    pub(crate) read_paths: Vec<PathBuf>,
    pub(crate) write_paths: Vec<PathBuf>,
    pub(crate) pass_env: Vec<String>,
    pub(crate) no_network: bool,
}

pub(crate) struct TimeoutResultCtx<'a> {
    pub(crate) command: &'a str,
    pub(crate) working_dir: &'a Path,
    pub(crate) execution_id: &'a str,
    pub(crate) timeout_secs: u64,
    pub(crate) total_lines: usize,
    pub(crate) total_bytes: usize,
    pub(crate) stdout: &'a str,
    pub(crate) stderr: &'a str,
    pub(crate) truncate_opts: &'a TruncateOpts,
    pub(crate) stdout_log_path: &'a Path,
    pub(crate) stderr_log_path: &'a Path,
}

pub(crate) struct SpawnResultCtx<'a> {
    pub(crate) execution_id: &'a str,
    pub(crate) command: &'a str,
    pub(crate) working_dir: &'a Path,
    pub(crate) stdout_log_path: &'a Path,
    pub(crate) stderr_log_path: &'a Path,
}

/// Per-execution temp directory, log paths, id, and opened log file handles.
pub(crate) struct ExecLog {
    pub(crate) exec_dir: PathBuf,
    pub(crate) stdout_log_path: PathBuf,
    pub(crate) stderr_log_path: PathBuf,
    pub(crate) execution_id: String,
    pub(crate) stdout_file: TokioFile,
    pub(crate) stderr_file: TokioFile,
}

/// Log file handles + their paths, passed to `run_to_completion`.
pub(crate) struct LogTargets<'a> {
    pub(crate) stdout_file: TokioFile,
    pub(crate) stderr_file: TokioFile,
    pub(crate) stdout_log_path: &'a Path,
    pub(crate) stderr_log_path: &'a Path,
}

/// Result of running a command to completion (exec path).
pub(crate) struct RunOutcome {
    pub(crate) status: std::process::ExitStatus,
    pub(crate) timed_out: bool,
    pub(crate) stdout_str: String,
    pub(crate) stderr_str: String,
}

/// Inputs for building a child command (shared by exec + spawn) before
/// the per-tool stdout/stderr `Stdio` destinations are supplied.
pub(crate) struct CommandBuildCtx<'a> {
    pub(crate) command: &'a str,
    pub(crate) working_dir: &'a Path,
    pub(crate) exec_dir: &'a Path,
    pub(crate) env: Option<&'a HashMap<String, String>>,
}

/// Inputs for assembling an "exited" tool result (shared by exec + wait).
pub(crate) struct ExitResultCtx<'a> {
    pub(crate) execution_id: &'a str,
    pub(crate) command: &'a str,
    pub(crate) working_dir: &'a Path,
    pub(crate) stdout_log_path: &'a Path,
    pub(crate) stderr_log_path: &'a Path,
    pub(crate) total_lines: usize,
    pub(crate) total_bytes: usize,
    pub(crate) exit_code: i32,
    pub(crate) streams_block: String,
    pub(crate) before_snaps: &'a [(PathBuf, gix::ObjectId)],
    pub(crate) snapshot_decision: &'a SnapshotDecision,
}

pub(crate) struct ReadExecLogSelection {
    pub(crate) lines: Vec<(usize, String)>,
    pub(crate) notices: Vec<String>,
}

pub(crate) struct ExecPreparation {
    pub(crate) working_dir: PathBuf,
    pub(crate) snapshot_decision: SnapshotDecision,
    pub(crate) before_snap_ids: Vec<(PathBuf, gix::ObjectId)>,
}

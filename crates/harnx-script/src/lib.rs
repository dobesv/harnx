//! Isolated script execution, independent of the harnx runtime.
//!
//! Each run starts in a fresh, in-memory `/tmp`, with `set -e` enabled. Scripts
//! can opt out with `set +e`. Only registered commands can reach host services;
//! the interpreter has no host filesystem, network, or Python capabilities.

mod bashkit_backend;
pub mod cli_map;
pub mod commands;
pub mod help;
mod schema;

use std::{collections::HashMap, path::Path, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub use bashkit::{FileSystem, FsLimits, InMemoryFs, ReadOnlyFs};
pub use bashkit_backend::BashKitBackend;
pub use tokio_util::sync::CancellationToken;

/// Resource limits for one invocation. Other interpreter/VFS limits retain
/// BashKit's defaults (including 10 MB per file and 100 MB total VFS storage).
#[derive(Clone, Debug)]
pub struct ScriptLimits {
    /// Wall time in seconds, defaulting to 24 hours. Zero disables the deadline
    /// in practice (100 years); larger values are capped at the same duration.
    pub timeout_secs: u64,
    pub max_commands: usize,
    pub max_stdout_bytes: usize,
    pub max_stderr_bytes: usize,
}

impl Default for ScriptLimits {
    fn default() -> Self {
        Self {
            timeout_secs: 86_400,
            max_commands: 10_000,
            max_stdout_bytes: 1_048_576,
            max_stderr_bytes: 1_048_576,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptStatus {
    Ok,
    Error,
    TimedOut,
    Cancelled,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScriptRun {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub limit_hit: Option<String>,
    pub status: ScriptStatus,
}

/// A trusted caller-supplied virtual filesystem, mounted at an absolute path.
/// Paths within `fs` are relative to its root. Wrap in [`ReadOnlyFs`] to deny
/// mutations. No host filesystem implementation is compiled into this crate.
pub struct FsMount {
    pub path: PathBuf,
    pub fs: Arc<dyn FileSystem>,
}

/// Context for a single command invocation, never shared mutable shell state.
pub struct ScriptCommandContext<'a> {
    pub env: &'a HashMap<String, String>,
    pub cwd: &'a Path,
    pub fs: Arc<dyn FileSystem>,
    /// Cancelled when this run ends, times out, fails fatally, or is dropped.
    /// Commands that spawn work must arrange cleanup using this signal. Prefer
    /// inline futures: dropping a run drops all interpreter-owned job futures.
    pub cancel: CancellationToken,
}

#[derive(Clone, Debug, Default)]
pub struct ScriptCommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// Registered async commands are shared across pipelines and background jobs.
/// Implementations must allow simultaneous calls; don't hold a lock over `run`.
#[async_trait]
pub trait ScriptCommand: Send + Sync {
    fn name(&self) -> &str;

    /// `stdin` contains exact pipeline bytes; args exclude the command name.
    /// Ordinary failures return a nonzero exit code. `Err(message)` is fatal:
    /// it stops the entire script, even under `set +e` or in a background job.
    async fn run(
        &self,
        args: &[String],
        stdin: Option<&[u8]>,
        ctx: ScriptCommandContext<'_>,
    ) -> Result<ScriptCommandOutput, String>;
}

#[async_trait]
pub trait ScriptBackend: Send + Sync {
    async fn run(
        &self,
        script: &str,
        limits: ScriptLimits,
        commands: Vec<Arc<dyn ScriptCommand>>,
        fs_mounts: Vec<FsMount>,
        cancel: CancellationToken,
    ) -> ScriptRun;
}

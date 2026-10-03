//! Launcher fixture whose MCP descendant stays in the inherited process group.

use super::Args;
use anyhow::Result;
use std::{
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};

pub(super) async fn run(args: &Args, dir: &Path) -> Result<()> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("--spawn-log")
        .arg(dir.join("descendant.pid"))
        .arg("--lifetime-lock")
        .arg(dir.join("descendant.lock"))
        .arg("--linger")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    for (flag, path) in [
        ("--script", args.script_path.as_deref().map(Path::new)),
        ("--request-log", args.request_log.as_deref()),
        ("--start-gate", args.start_gate.as_deref()),
    ] {
        if let Some(path) = path {
            command.arg(flag).arg(path);
        }
    }
    // Deliberately not manager-spawned: real launchers leave descendants in the
    // bridge-owned group. No parent-death signal or child Drop may hide leaks.
    let mut child = command.spawn()?;
    if args.launcher_exit {
        return Ok(());
    }
    // Failure backstop, well beyond the test's cleanup assertion deadline.
    tokio::time::sleep(Duration::from_secs(120)).await;
    let _ = child.kill();
    child.wait()?;
    Ok(())
}

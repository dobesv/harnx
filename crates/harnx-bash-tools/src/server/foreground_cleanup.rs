//! Signals use the owned, unreaped child identity, never a persisted/reusable PID.
use process_wrap::tokio::ChildWrapper;
use std::time::Duration;

pub(super) struct ProcessIdentity {
    pid: Option<u32>,
    #[cfg(target_os = "linux")]
    started: Option<String>,
}

impl ProcessIdentity {
    pub fn capture(child: &dyn ChildWrapper) -> Self {
        let pid = child.id();
        Self {
            pid,
            #[cfg(target_os = "linux")]
            started: pid.and_then(start_time),
        }
    }

    fn validate(&self, child: &dyn ChildWrapper) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.pid.is_some() && child.id() == self.pid,
            "owned child already reaped; refusing reusable process-group ID"
        );
        #[cfg(target_os = "linux")]
        anyhow::ensure!(
            self.started.is_some() && self.pid.and_then(start_time) == self.started,
            "process identity unavailable or changed; cleanup unconfirmed"
        );
        Ok(())
    }

    pub async fn terminate(&self, child: &mut dyn ChildWrapper) -> anyhow::Result<()> {
        self.validate(child)?;
        #[cfg(unix)]
        {
            if let Err(error) = child.signal(libc::SIGTERM) {
                log::debug!("process-group SIGTERM failed; still attempting kill: {error}");
            }
            // Don't reap the group leader during grace. Its reserved PID keeps
            // escalation from ever signalling an unrelated reused group ID.
            tokio::time::sleep(Duration::from_millis(150)).await;
            self.validate(child)?;
        }
        kill_unless_exited(child).await
    }
}

async fn kill_unless_exited(child: &mut dyn ChildWrapper) -> anyhow::Result<()> {
    let Err(error) = child.start_kill() else {
        return Ok(());
    };
    // Darwin can report EPERM for an exited group. Check exit evidence only
    // after the last signal attempt: reaping releases the reserved PGID.
    if is_missing_process_error(&error) || exit_is_ready(child).await {
        log::debug!("process-group kill no longer needed: {error}");
        return Ok(());
    }
    Err(error.into())
}

async fn exit_is_ready(child: &mut dyn ChildWrapper) -> bool {
    // Group try_wait can reap without updating Tokio's kill-on-drop state.
    // Poll wait instead, without a cooperative-budget yield hiding a ready exit.
    let wait = tokio::task::unconstrained(child.wait());
    matches!(tokio::time::timeout(Duration::ZERO, wait).await, Ok(Ok(_)))
}

fn is_missing_process_error(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::InvalidInput {
        return true;
    }
    #[cfg(unix)]
    if matches!(error.raw_os_error(), Some(libc::ESRCH | libc::ECHILD)) {
        return true;
    }
    false
}

#[cfg(target_os = "linux")]
fn start_time(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm may contain spaces or ')'; starttime is field 22, after comm/state.
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)
        .map(str::to_owned)
}

#[cfg(all(test, unix))]
mod tests;

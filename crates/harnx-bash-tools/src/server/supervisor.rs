//! One task owns each spawned background process for its whole life.
//!
//! Callers never hold the child: a wait watches the exit channel and a
//! terminate sends a control message. Dropping either of those futures leaves
//! the process and its supervisor untouched, so a cancelled or interrupted wait
//! can't make the process unreachable, and any number of waits can watch the
//! same exit.
use super::foreground_cleanup::ProcessIdentity;
use process_wrap::tokio::ChildWrapper;
use std::process::ExitStatus;
use std::time::Duration;
use tokio::sync::{mpsc, watch};

/// How long to back off before waiting again after a failed wait. A failed
/// wait doesn't prove the process exited, so the supervisor keeps waiting.
const WAIT_RETRY_DELAY: Duration = Duration::from_secs(1);

/// Failed waits in a row after which the supervisor gives up. A successful
/// wait ends supervision, so every failure counts towards this. An error that
/// persists, such as the leader having been reaped by someone else, would
/// otherwise keep the process "running" forever with the cause hidden.
const MAX_WAIT_FAILURES: u32 = 5;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProcessExit {
    pub(crate) code: Option<i32>,
    /// The supervisor killed the process group at its deadline.
    pub(crate) timed_out: bool,
    /// The supervisor gave up because waiting kept failing, so the process's
    /// real fate is unknown. Waits report this as an error.
    pub(crate) wait_error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProcessControl {
    /// Deliver this signal to the process group.
    #[cfg(unix)]
    Signal(i32),
    /// Kill the process group, or the job object on Windows.
    Kill,
}

/// Hands `child` to a new supervisor task, which kills its process group once
/// `deadline` passes. The receiver reads `None` until the process exits.
pub(crate) fn spawn_supervisor(
    child: Box<dyn ChildWrapper>,
    deadline: Duration,
) -> (
    watch::Receiver<Option<ProcessExit>>,
    mpsc::UnboundedSender<ProcessControl>,
) {
    let (exit_tx, exit_rx) = watch::channel(None);
    let (control_tx, control_rx) = mpsc::unbounded_channel();
    let supervisor = Supervisor {
        identity: ProcessIdentity::capture(child.as_ref()),
        child,
        control: control_rx,
        wait_delay: Duration::ZERO,
        wait_failures: 0,
        timed_out: false,
    };
    // Detached on purpose: the task, not any caller's future, owns the child.
    tokio::spawn(async move {
        let exit = supervisor.run(deadline).await;
        exit_tx.send_replace(Some(exit));
    });
    (exit_rx, control_tx)
}

struct Supervisor {
    child: Box<dyn ChildWrapper>,
    identity: ProcessIdentity,
    control: mpsc::UnboundedReceiver<ProcessControl>,
    /// Back-off before the next wait, set only by the failure just before it.
    wait_delay: Duration,
    wait_failures: u32,
    timed_out: bool,
}

impl Supervisor {
    async fn run(mut self, deadline: Duration) -> ProcessExit {
        let expiry = tokio::time::sleep(deadline);
        tokio::pin!(expiry);
        loop {
            let wait_delay = std::mem::take(&mut self.wait_delay);
            tokio::select! {
                status = wait_after(self.child.as_mut(), wait_delay) => {
                    if let Some(exit) = self.observe(status) {
                        return exit;
                    }
                }
                Some(message) = self.control.recv() => self.deliver(message),
                () = &mut expiry, if !self.timed_out => self.expire().await,
            }
        }
    }

    fn observe(&mut self, status: std::io::Result<ExitStatus>) -> Option<ProcessExit> {
        match status {
            Ok(status) => Some(ProcessExit {
                code: status.code(),
                timed_out: self.timed_out,
                wait_error: None,
            }),
            Err(error) => self.wait_failed(error),
        }
    }

    fn wait_failed(&mut self, error: std::io::Error) -> Option<ProcessExit> {
        self.wait_failures += 1;
        if self.wait_failures >= MAX_WAIT_FAILURES {
            log::warn!("waiting on a spawned process keeps failing; giving up: {error}");
            return Some(ProcessExit {
                code: None,
                timed_out: false,
                wait_error: Some(error.to_string()),
            });
        }
        log::warn!("waiting on a spawned process failed; waiting again: {error}");
        self.wait_delay = WAIT_RETRY_DELAY;
        None
    }

    fn deliver(&mut self, message: ProcessControl) {
        // A reaped leader's group ID can be reused by an unrelated process.
        if self.child.id().is_none() {
            return;
        }
        let result = match message {
            #[cfg(unix)]
            ProcessControl::Signal(signal) => self.child.signal(signal),
            ProcessControl::Kill => self.child.start_kill(),
        };
        if let Err(error) = result {
            log::warn!("failed to deliver {message:?} to a spawned process: {error}");
        }
    }

    /// Kills the process group at its deadline. The exit is still recorded
    /// by `observe`, once the leader has actually been reaped.
    async fn expire(&mut self) {
        self.timed_out = true;
        if let Err(error) = self.identity.terminate(self.child.as_mut()).await {
            harnx_toolset::cleanup::unconfirmed(format!(
                "spawned process deadline cleanup: {error:#}"
            ));
        }
    }
}

async fn wait_after(child: &mut dyn ChildWrapper, delay: Duration) -> std::io::Result<ExitStatus> {
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
    child.wait().await
}

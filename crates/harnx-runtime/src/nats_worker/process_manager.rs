//! Stable OS-thread parent for worker-managed child processes.
//!
//! Linux ties `PR_SET_PDEATHSIG` to the thread that calls `fork`, not merely
//! to that thread's process. Spawning directly from a Tokio worker is unsafe:
//! `block_in_place` can hand that worker to the blocking pool, whose idle
//! threads retire and make healthy children believe their parent died. This
//! manager serializes process creation through one ordinary thread that lives
//! for as long as its owning supervisor.

use std::io;
use std::sync::{mpsc, Arc, OnceLock};
use std::thread::JoinHandle;
use tokio::process::{Child, Command};
use tokio::sync::oneshot;

#[derive(Clone)]
pub(super) struct ChildProcessManager {
    inner: Arc<ManagerInner>,
}

struct ManagerInner {
    started: OnceLock<StartedManager>,
}

struct StartedManager {
    sender: Option<mpsc::Sender<ManagerRequest>>,
    thread: Option<JoinHandle<()>>,
    startup_error: Option<String>,
}

enum ManagerRequest {
    Spawn {
        command: Box<Command>,
        runtime: tokio::runtime::Handle,
        response: oneshot::Sender<io::Result<Child>>,
    },
    Shutdown,
}

impl ChildProcessManager {
    pub(super) fn new() -> Self {
        Self {
            inner: Arc::new(ManagerInner {
                started: OnceLock::new(),
            }),
        }
    }

    /// Spawn `command` from the manager's stable OS thread and return a Tokio
    /// child handle that callers may monitor from any runtime task.
    pub(super) async fn spawn(&self, mut command: Command) -> io::Result<Child> {
        configure_managed_process(&mut command);
        let runtime = tokio::runtime::Handle::try_current().map_err(|error| {
            io::Error::other(format!(
                "spawn managed child outside a Tokio runtime: {error}"
            ))
        })?;
        let manager = self.inner.started.get_or_init(start_manager);
        let sender = manager.sender.as_ref().ok_or_else(|| {
            io::Error::other(format!(
                "child process manager failed to start: {}",
                manager.startup_error.as_deref().unwrap_or("unknown error")
            ))
        })?;
        let (response, result) = oneshot::channel();
        sender
            .send(ManagerRequest::Spawn {
                command: Box::new(command),
                runtime,
                response,
            })
            .map_err(|_| io::Error::other("child process manager stopped unexpectedly"))?;
        result
            .await
            .map_err(|_| io::Error::other("child process manager stopped during spawn"))?
    }
}

fn start_manager() -> StartedManager {
    let (sender, receiver) = mpsc::channel();
    match std::thread::Builder::new()
        .name("harnx-process-manager".to_string())
        .spawn(move || run_manager(receiver))
    {
        Ok(thread) => StartedManager {
            sender: Some(sender),
            thread: Some(thread),
            startup_error: None,
        },
        Err(error) => StartedManager {
            sender: None,
            thread: None,
            startup_error: Some(error.to_string()),
        },
    }
}

impl Drop for StartedManager {
    fn drop(&mut self) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(ManagerRequest::Shutdown);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run_manager(receiver: mpsc::Receiver<ManagerRequest>) {
    while let Ok(request) = receiver.recv() {
        match request {
            ManagerRequest::Spawn {
                mut command,
                runtime,
                response,
            } => {
                // Tokio builds its async child reaper after std::process::Command
                // performs the actual spawn. Enter the caller's runtime here so
                // the returned Child remains compatible with its async monitor,
                // while the kernel still sees this stable thread as the parent.
                let _guard = runtime.enter();
                let _ = response.send(command.spawn());
            }
            ManagerRequest::Shutdown => break,
        }
    }
}

#[cfg(unix)]
fn configure_managed_process(command: &mut Command) {
    #[cfg(target_os = "linux")]
    let parent_pid = std::process::id() as libc::pid_t;
    // SAFETY: pre_exec invokes only async-signal-safe libc calls.
    unsafe {
        command.pre_exec(move || {
            if libc::setpgid(0, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            #[cfg(target_os = "linux")]
            {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::getppid() != parent_pid {
                    libc::raise(libc::SIGTERM);
                }
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn configure_managed_process(_command: &mut Command) {}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::process::Stdio;
    use std::time::Duration;

    fn long_running_command() -> Command {
        let mut command = Command::new("sleep");
        command
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        command
    }

    #[test]
    fn managed_child_survives_tokio_blocking_thread_retirement() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_keep_alive(Duration::from_millis(25))
            .enable_all()
            .build()
            .expect("build test runtime");
        runtime.block_on(async {
            let manager = ChildProcessManager::new();
            let mut child = manager
                .spawn(long_running_command())
                .await
                .expect("spawn managed child");

            tokio::task::block_in_place(|| std::thread::sleep(Duration::from_millis(200)));
            tokio::time::sleep(Duration::from_millis(200)).await;

            assert_eq!(
                child.try_wait().expect("poll managed child"),
                None,
                "retiring a Tokio blocking thread must not terminate its managed child"
            );
            child.start_kill().expect("kill managed child");
            child.wait().await.expect("reap managed child");
        });
    }

    #[test]
    fn dropping_manager_terminates_its_children() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build test runtime");
        runtime.block_on(async {
            let manager = ChildProcessManager::new();
            assert!(
                manager.inner.started.get().is_none(),
                "constructing a manager must not start its thread"
            );
            let mut child = manager
                .spawn(long_running_command())
                .await
                .expect("spawn managed child");
            assert!(manager.inner.started.get().is_some());

            drop(manager);

            tokio::time::timeout(Duration::from_secs(2), child.wait())
                .await
                .expect("parent-death signal should terminate child")
                .expect("reap terminated child");
        });
    }
}

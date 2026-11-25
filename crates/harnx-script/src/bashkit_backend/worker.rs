use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

use super::{BashKitBackend, RunControl};
use crate::{CancellationToken, ScriptBackend, ScriptLimits, ScriptRun, ScriptStatus};

// A parent holds its permit while awaiting host calls, so descendants must use
// a different pool. Fair pools at increasing depths make the wait graph acyclic.
// Permits live in closures, including after an unstarted queued caller returns.
pub(super) const MAX_SCRIPT_WORKERS: usize = 8;
pub(super) static ADMISSION: LazyLock<[Arc<Semaphore>; crate::MAX_SCRIPT_NESTING]> =
    LazyLock::new(|| std::array::from_fn(|_| Arc::new(Semaphore::new(MAX_SCRIPT_WORKERS))));

tokio::task_local! {
    // Depth for the next run, explicitly restored inside the blocking worker.
    pub(crate) static NESTING_DEPTH: usize;
}

struct CancelOnDrop {
    control: Arc<RunControl>,
    worker: tokio::task::AbortHandle,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.control.cancel();
        self.worker.abort();
    }
}

struct Completed {
    result: ScriptRun,
    at: Instant,
}

impl Completed {
    fn deadline_hit(&self, deadline: Instant, stopped: bool) -> bool {
        if self.result.status == ScriptStatus::TimedOut {
            return true;
        }
        // BashKit reports Monty's own time limit as exit 124 with this line, and
        // only when the VM's check fired; a script's own `TimeoutError` is an
        // ordinary traceback. The limit is the run's remaining time, so it is the
        // run deadline, even when the VM's timer fires a little before this
        // one does (a coarse clock does it on Windows).
        if self.result.exit_code == 124
            && self.result.error.is_none()
            && self
                .result
                .stderr
                .lines()
                .any(|line| line.starts_with("python3: execution timed out after "))
        {
            return true;
        }
        // Completion time belongs to the worker, not its starved parent.
        self.at >= deadline && stopped
    }
}

type Worker = tokio::task::JoinHandle<Completed>;

#[async_trait]
impl ScriptBackend for BashKitBackend {
    async fn run(&self, request: crate::ScriptRequest<'_>) -> ScriptRun {
        let timeout = super::execution_limits(&request.limits).timeout;
        let deadline = Instant::now() + timeout;
        let limits = request.limits.clone();
        let cancel = request.cancel.clone();
        let depth = NESTING_DEPTH.try_with(|depth| *depth).unwrap_or(0);
        let permit = match admit(&cancel, deadline, timeout, depth).await {
            Ok(permit) => permit,
            Err(result) => return finish(result, &limits),
        };
        let control = Arc::new(RunControl {
            stop: cancel.child_token(),
            fatal: Mutex::new(None),
            flag: OnceLock::new(),
            host_stopped: std::sync::atomic::AtomicBool::new(false),
            child_depth: depth + 1,
        });
        let mut worker = spawn(request, control.clone(), permit);
        let _cleanup = CancelOnDrop {
            control: control.clone(),
            worker: worker.abort_handle(),
        };
        let (result, stopped_by_deadline) = tokio::select! {
            biased;
            () = cancel.cancelled() => (stop_worker(&control, worker).await, false),
            result = &mut worker => (result, false),
            () = tokio::time::sleep_until(deadline) => (stop_worker(&control, worker).await, true),
        };
        let completed = result.unwrap_or_else(|error| Completed {
            result: worker_error(error),
            at: Instant::now(),
        });
        let deadline_hit = completed.deadline_hit(deadline, stopped_by_deadline);
        let mut result = completed.result;
        if control
            .host_stopped
            .load(std::sync::atomic::Ordering::Acquire)
        {
            result.status = ScriptStatus::Stopped;
            result.exit_code = super::INTERRUPTED_EXIT_CODE;
        } else if !cancel.is_cancelled() && deadline_hit {
            normalize_timeout(&mut result, timeout);
        }
        finish(result, &limits)
    }
}

async fn admit(
    cancel: &CancellationToken,
    deadline: Instant,
    timeout: Duration,
    depth: usize,
) -> Result<OwnedSemaphorePermit, ScriptRun> {
    let Some(pool) = ADMISSION.get(depth) else {
        return Err(worker_failure(
            false,
            format!(
                "script nesting depth limit exceeded (maximum {} runs)\n",
                crate::MAX_SCRIPT_NESTING
            ),
        ));
    };
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(worker_failure(true, "cancelled\n".to_owned())),
        () = tokio::time::sleep_until(deadline) => {
            let mut result = worker_failure(true, "cancelled\n".to_owned());
            normalize_timeout(&mut result, timeout);
            Err(result)
        }
        permit = pool.clone().acquire_owned() => Ok(permit.expect("script admission never closes")),
    }
}

fn spawn(
    request: crate::ScriptRequest<'_>,
    control: Arc<RunControl>,
    permit: OwnedSemaphorePermit,
) -> Worker {
    let crate::ScriptRequest {
        script,
        limits,
        commands,
        python_functions,
        fs_mounts,
        cancel: _,
    } = request;
    let script = script.to_owned();
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let result = runtime.block_on(NESTING_DEPTH.scope(
            control.child_depth,
            BashKitBackend.execute(
                crate::ScriptRequest {
                    script: &script,
                    limits,
                    commands,
                    python_functions,
                    fs_mounts,
                    cancel: control.stop.clone(),
                },
                control,
            ),
        ));
        Completed {
            result,
            at: Instant::now(),
        }
    })
}

async fn stop_worker(
    control: &RunControl,
    worker: Worker,
) -> Result<Completed, tokio::task::JoinError> {
    // Drain completed work before publishing a stop. Its timestamp also closes
    // the race between this check and the worker finishing before cancellation.
    if worker.is_finished() {
        return worker.await;
    }
    control.cancel();
    worker.abort();
    if control.flag.get().is_none() {
        // Tokio retains aborted queued captures until a pool thread drains them.
        // Their admission permits stay held, so repeated cancellations stay bounded.
        Ok(Completed {
            result: worker_failure(true, "cancelled\n".to_owned()),
            at: Instant::now(),
        })
    } else {
        // A native VM operation may ignore cancellation. Detaching would allow
        // interpreter-owned tools to resume after the caller reports completion.
        worker.await
    }
}

fn normalize_timeout(result: &mut ScriptRun, timeout: Duration) {
    let message = format!("execution timeout exceeded ({timeout:?})");
    if result.stderr == "cancelled\n" || result.stderr == "execution cancelled\n" {
        result.stderr = format!("{message}\n");
    }
    if result.error.is_none()
        || matches!(
            result.status,
            ScriptStatus::Cancelled | ScriptStatus::TimedOut
        )
    {
        result.error = Some(message.clone());
    }
    result.status = ScriptStatus::TimedOut;
    result.exit_code = 124;
    result.limit_hit = Some(message);
}

fn finish(mut result: ScriptRun, limits: &ScriptLimits) -> ScriptRun {
    let mut capture = super::Capture::new(limits);
    capture.append("", &result.stderr);
    result.stderr = capture.stderr.text;
    result.stderr_truncated |= capture.stderr.truncated;
    result
}

fn worker_error(error: tokio::task::JoinError) -> ScriptRun {
    let cancelled = error.is_cancelled();
    let message = if cancelled {
        "cancelled\n".to_owned()
    } else {
        format!("script worker failed: {error}\n")
    };
    worker_failure(cancelled, message)
}

fn worker_failure(cancelled: bool, message: String) -> ScriptRun {
    ScriptRun {
        stdout: String::new(),
        error: Some(message.trim_end().to_owned()),
        stderr: message,
        exit_code: if cancelled { 130 } else { 1 },
        stdout_truncated: false,
        stderr_truncated: false,
        limit_hit: None,
        status: if cancelled {
            ScriptStatus::Cancelled
        } else {
            ScriptStatus::Error
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completed(exit_code: i32, stderr: &str, at: Instant) -> Completed {
        Completed {
            result: ScriptRun {
                stdout: String::new(),
                stderr: stderr.to_owned(),
                exit_code,
                stdout_truncated: false,
                stderr_truncated: false,
                limit_hit: None,
                error: None,
                status: ScriptStatus::Error,
            },
            at,
        }
    }

    #[test]
    fn a_vm_timeout_that_finishes_just_before_the_deadline_is_the_deadline() {
        let deadline = Instant::now() + Duration::from_secs(1);
        let early = deadline - Duration::from_millis(15);
        let vm_timeout = completed(124, "python3: execution timed out after 0.8s\n", early);
        assert!(vm_timeout.deadline_hit(deadline, false));
    }

    #[test]
    fn a_script_error_before_the_deadline_is_not_a_timeout() {
        let deadline = Instant::now() + Duration::from_secs(1);
        let early = deadline - Duration::from_millis(500);
        // Exit 1 with a traceback is what a script's own `TimeoutError` is.
        let raised = completed(1, "TimeoutError: time limit exceeded: 0.8s\n", early);
        assert!(!raised.deadline_hit(deadline, false));
        // A bare exit 124 without the VM's line isn't one either.
        assert!(!completed(124, "", early).deadline_hit(deadline, false));
    }

    #[test]
    fn a_run_the_deadline_stopped_is_a_timeout() {
        let deadline = Instant::now();
        let after = deadline + Duration::from_millis(5);
        assert!(completed(130, "", after).deadline_hit(deadline, true));
        assert!(!completed(130, "", after - Duration::from_millis(10)).deadline_hit(deadline, true));
    }
}

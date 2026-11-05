use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

use super::{BashKitBackend, RunControl};
use crate::{CancellationToken, ScriptBackend, ScriptLimits, ScriptRun, ScriptStatus};

// Cap running interpreters and retained closures in Tokio's blocking queue.
// The permit lives in the closure, including after a queued caller returns.
pub(super) const MAX_SCRIPT_WORKERS: usize = 8;
pub(super) static ADMISSION: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(MAX_SCRIPT_WORKERS)));

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
        // Completion time belongs to the worker, not its starved parent.
        if self.at < deadline {
            return false;
        }
        if stopped {
            return true;
        }
        if self.result.exit_code == 0 || self.result.error.is_some() {
            return false;
        }
        // BashKit 0.20.2 exposes Monty's VM timeout only as stderr/exit 1.
        self.result
            .stderr
            .lines()
            .any(|line| line.starts_with("TimeoutError: time limit exceeded:"))
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
        let permit = match admit(&cancel, deadline, timeout).await {
            Ok(permit) => permit,
            Err(result) => return finish(result, &limits),
        };
        let control = Arc::new(RunControl {
            stop: cancel.child_token(),
            fatal: Mutex::new(None),
            flag: OnceLock::new(),
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
        if !cancel.is_cancelled() && deadline_hit {
            normalize_timeout(&mut result, timeout);
        }
        finish(result, &limits)
    }
}

async fn admit(
    cancel: &CancellationToken,
    deadline: Instant,
    timeout: Duration,
) -> Result<OwnedSemaphorePermit, ScriptRun> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(worker_failure(true, "cancelled\n".to_owned())),
        () = tokio::time::sleep_until(deadline) => {
            let mut result = worker_failure(true, "cancelled\n".to_owned());
            normalize_timeout(&mut result, timeout);
            Err(result)
        }
        permit = ADMISSION.clone().acquire_owned() => Ok(permit.expect("script admission never closes")),
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
        fs_mounts,
        cancel: _,
    } = request;
    let script = script.to_owned();
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let result = runtime.block_on(BashKitBackend.execute(
            crate::ScriptRequest {
                script: &script,
                limits,
                commands,
                fs_mounts,
                cancel: control.stop.clone(),
            },
            control,
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

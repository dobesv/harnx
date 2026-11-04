use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;

use super::{BashKitBackend, RunControl};
use crate::{ScriptBackend, ScriptRun, ScriptStatus};

struct CancelOnDrop {
    control: Arc<RunControl>,
    worker: tokio::task::AbortHandle,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.control.cancel();
        // Aborting cannot stop a running blocking task, but removes queued work.
        self.worker.abort();
    }
}

#[async_trait]
impl ScriptBackend for BashKitBackend {
    async fn run(&self, request: crate::ScriptRequest<'_>) -> ScriptRun {
        let crate::ScriptRequest {
            script,
            limits,
            commands,
            fs_mounts,
            cancel,
        } = request;
        let control = Arc::new(RunControl {
            stop: cancel.child_token(),
            fatal: Mutex::new(None),
            flag: OnceLock::new(),
        });
        let worker_control = control.clone();
        let worker_limits = limits.clone();
        let timeout = super::execution_limits(&limits).timeout;
        let deadline = tokio::time::Instant::now() + timeout;
        let script = script.to_owned();
        let runtime = tokio::runtime::Handle::current();
        // Monty executes synchronous VM slices. Keep the parent executor free
        // to set the flag checked by those slices, even on a single-thread runtime.
        let mut worker = tokio::task::spawn_blocking(move || {
            runtime.block_on(BashKitBackend.execute(
                crate::ScriptRequest {
                    script: &script,
                    limits: worker_limits,
                    commands,
                    fs_mounts,
                    cancel: worker_control.stop.clone(),
                },
                worker_control,
            ))
        });
        let _cleanup = CancelOnDrop {
            control: control.clone(),
            worker: worker.abort_handle(),
        };
        let result = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                stop_worker(&control, worker).await
            }
            () = tokio::time::sleep_until(deadline) => {
                stop_worker(&control, worker).await
            }
            result = &mut worker => result,
        };
        let mut result = result.unwrap_or_else(worker_error);
        // Monty can return a plain TimeoutError just before BashKit notices its
        // own deadline. Use the host deadline, not exception-text matching, to
        // report the run limit consistently (including a saturated worker pool).
        if !cancel.is_cancelled() && tokio::time::Instant::now() >= deadline {
            // Running interpreters and queued workers use different cancellation
            // messages. Normalize by status, preserving unrelated tool/panic errors.
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
        // Worker failures and queued cancellation don't pass through the
        // interpreter's capture. Apply the same cap to their diagnostics.
        let mut capture = super::Capture::new(&limits);
        capture.append("", &result.stderr);
        result.stderr = capture.stderr.text;
        result.stderr_truncated |= capture.stderr.truncated;
        result
    }
}

async fn stop_worker(
    control: &RunControl,
    worker: tokio::task::JoinHandle<ScriptRun>,
) -> Result<ScriptRun, tokio::task::JoinError> {
    control.cancel();
    worker.abort();
    if control.flag.get().is_none() {
        // Tokio only drains aborted blocking tasks when a pool thread is free.
        // No interpreter has started. The published stop forbids later entry,
        // so returning now cannot leave Python or tool commands running.
        Ok(worker_failure(true, "cancelled\n".to_owned()))
    } else {
        // Running blocking tasks cannot be aborted; join cooperative VM/job cleanup.
        worker.await
    }
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

use std::{
    sync::{atomic::AtomicBool, atomic::Ordering, Arc, Mutex, OnceLock},
    time::Duration,
};

use async_trait::async_trait;
use bashkit::{Builtin, BuiltinContext, Error, ExecResult, ExecutionLimits};

use crate::{
    CancellationToken, InMemoryFs, ScriptCommand, ScriptCommandContext, ScriptLimits, ScriptRun,
};

mod capture;
mod outcome;
mod python;
mod resolver;
mod shell;
mod worker;
use capture::Capture;
use shell::build_shell;
pub(crate) use worker::NESTING_DEPTH;

// Shell convention: 128 + SIGINT, shared by cancellation and host-requested stops.
const INTERRUPTED_EXIT_CODE: i32 = 130;

/// Stateless factory: BashKit cancellation flags and shell state never cross runs.
#[derive(Clone, Copy, Debug, Default)]
pub struct BashKitBackend;

// Avoid overflowing Instant on any supported platform. BashKit requires a finite
// Duration; a century is effectively unlimited for an individual tool call.
const NO_DEADLINE: Duration = Duration::from_secs(100 * 365 * 24 * 60 * 60);

impl BashKitBackend {
    fn builder(limits: &ScriptLimits) -> bashkit::BashBuilder {
        bashkit::Bash::builder()
            .fs(Arc::new(InMemoryFs::new()))
            .cwd("/tmp")
            .limits(execution_limits(limits))
            .python_with_limits(python_limits(limits))
            .env("BASHKIT_ALLOW_INPROCESS_PYTHON", "1")
            // Defense against accidental Cargo feature unification enabling HTTP.
            // Network access is only through explicitly registered host commands.
            .builtin_filter(|name| !matches!(name, "curl" | "wget" | "http"))
    }

    /// Names compiled into this backend, excluding caller-registered commands.
    pub fn builtin_names(&self) -> Vec<String> {
        Self::builder(&ScriptLimits::default())
            .build()
            .builtin_names()
    }
}

fn execution_limits(limits: &ScriptLimits) -> ExecutionLimits {
    ExecutionLimits::default()
        .timeout(if limits.timeout_secs == 0 {
            NO_DEADLINE
        } else {
            Duration::from_secs(limits.timeout_secs).min(NO_DEADLINE)
        })
        .max_commands(limits.max_commands)
        .max_stdout_bytes(limits.max_stdout_bytes)
        .max_stderr_bytes(limits.max_stderr_bytes)
}

fn python_limits(limits: &ScriptLimits) -> bashkit::PythonLimits {
    bashkit::PythonLimits::default()
        .max_duration(execution_limits(limits).timeout)
        .max_memory(limits.max_python_memory_bytes)
}

pub(crate) struct RunControl {
    stop: CancellationToken,
    fatal: Mutex<Option<String>>,
    flag: OnceLock<Arc<AtomicBool>>,
    host_stopped: AtomicBool,
    // Admission depth inherited by runs started from this run's host callbacks.
    pub(crate) child_depth: usize,
}

impl RunControl {
    pub(crate) fn stop_from_host(&self) {
        self.host_stopped.store(true, Ordering::Release);
        self.cancel();
    }

    fn cancel(&self) {
        // Publish the stop first: a worker installing its flag concurrently
        // must see either this token or the flag write below.
        self.stop.cancel();
        if let Some(flag) = self.flag.get() {
            flag.store(true, Ordering::Release);
        }
    }
}

impl Drop for RunControl {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

struct CommandAdapter {
    command: Arc<dyn ScriptCommand>,
    control: Arc<RunControl>,
}

#[async_trait]
impl Builtin for CommandAdapter {
    async fn execute(&self, ctx: BuiltinContext<'_>) -> bashkit::Result<ExecResult> {
        let context = ScriptCommandContext {
            env: ctx.env,
            cwd: ctx.cwd,
            fs: ctx.fs.clone(),
            cancel: self.control.stop.clone(),
            control: crate::ScriptControl {
                inner: self.control.clone(),
            },
        };
        let result = self.command.run(ctx.args, ctx.stdin_bytes(), context).await;
        if self.control.host_stopped.load(Ordering::Acquire) {
            // Yield to the run's stop branch instead of resuming script code.
            return std::future::pending().await;
        }
        match result {
            Ok(result) => Ok(ExecResult {
                stdout: result.stdout.into(),
                stderr: result.stderr.into(),
                exit_code: result.exit_code,
                ..Default::default()
            }),
            Err(message) => {
                // BashKit turns background-job errors into exit codes. An
                // out-of-band stop preserves fatal semantics under `set +e`.
                self.control.fatal.lock().unwrap().get_or_insert(message);
                self.control.cancel();
                Err(Error::Cancelled)
            }
        }
    }
}

impl BashKitBackend {
    async fn execute(
        &self,
        request: crate::ScriptRequest<'_>,
        control: Arc<RunControl>,
    ) -> ScriptRun {
        let crate::ScriptRequest {
            script,
            limits,
            commands,
            python_functions,
            fs_mounts,
            cancel: _,
        } = request;
        let mut bash = build_shell(&limits, commands, python_functions, &control);
        let flag = bash.cancellation_token();
        control.flag.set(flag.clone()).unwrap();
        if control.stop.is_cancelled() {
            flag.store(true, Ordering::Release);
        }
        // BashKit resets shell options between exec calls. Keep errexit in the
        // same execution as the user's script; diagnostics have one extra line.
        let script = format!("set -e\n{script}");
        let captured = Arc::new(Mutex::new(Capture::new(&limits)));
        let streamed = captured.clone();
        let output = Box::new(
            move |stdout: &bashkit::StreamData, stderr: &bashkit::StreamData| {
                streamed
                    .lock()
                    .unwrap()
                    .append(&stdout.text_lossy(), &stderr.text_lossy());
            },
        );

        let result = async {
            for mount in fs_mounts {
                bash.mount(mount.path, mount.fs)?;
            }
            bash.exec_streaming(&script, output).await
        };
        let result = tokio::select! {
            biased;
            () = control.stop.cancelled() => {
                flag.store(true, Ordering::Release);
                Err(Error::Cancelled)
            }
            result = result => result,
        };
        // Cancelling the flag alone only stops at command boundaries. Dropping
        // Bash drops all job futures too, including commands awaiting host I/O.
        flag.store(true, Ordering::Release);
        control.stop.cancel();
        drop(bash);
        let fatal = control.fatal.lock().unwrap().take();
        let mut capture = captured.lock().unwrap();
        outcome::finish(
            result,
            &mut capture,
            fatal,
            control.host_stopped.load(Ordering::Acquire),
        )
    }
}

#[cfg(test)]
mod tests;

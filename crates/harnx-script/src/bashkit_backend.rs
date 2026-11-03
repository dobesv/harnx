use std::{
    sync::{atomic::AtomicBool, atomic::Ordering, Arc, Mutex, OnceLock},
    time::Duration,
};

use async_trait::async_trait;
use bashkit::{Builtin, BuiltinContext, Error, ExecResult, ExecutionLimits};

use crate::{
    CancellationToken, InMemoryFs, ScriptBackend, ScriptCommand, ScriptCommandContext,
    ScriptLimits, ScriptRun, ScriptStatus,
};

mod capture;
mod outcome;
mod resolver;
mod shell;
use capture::Capture;
use outcome::classify;
use shell::build_shell;

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
            // Defense against accidental Cargo feature unification enabling HTTP.
            // No host mounts, transports, or embedded runtimes are configured.
            .builtin_filter(|name| !matches!(name, "curl" | "wget"))
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

struct RunControl {
    stop: CancellationToken,
    fatal: Mutex<Option<String>>,
    flag: OnceLock<Arc<AtomicBool>>,
}

impl RunControl {
    fn cancel(&self) {
        if let Some(flag) = self.flag.get() {
            flag.store(true, Ordering::Release);
        }
        self.stop.cancel();
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
        };
        match self.command.run(ctx.args, ctx.stdin_bytes(), context).await {
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
        // This guard also cancels command-owned work if the caller drops `run`.
        let _cleanup = control.stop.clone().drop_guard();
        let mut bash = build_shell(&limits, commands, &control);
        let flag = bash.cancellation_token();
        control.flag.set(flag.clone()).unwrap();
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
        let (status, exit_code, limit_hit) = if let Some(message) = fatal {
            capture.append("", &format!("{message}\n"));
            (ScriptStatus::Error, 1, None)
        } else {
            classify(result, &mut capture)
        };
        ScriptRun {
            stdout: std::mem::take(&mut capture.stdout.text),
            stderr: std::mem::take(&mut capture.stderr.text),
            stdout_truncated: capture.stdout.truncated,
            stderr_truncated: capture.stderr.truncated,
            exit_code,
            status,
            limit_hit,
        }
    }
}

#[cfg(test)]
mod tests;
